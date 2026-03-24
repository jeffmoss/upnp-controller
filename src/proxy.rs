//! TCP/UDP proxy for forwarding external traffic to cluster services.
//!
//! Each proxy listens on a local port (on the controller's node) and forwards
//! connections/packets to a cluster-internal target (ClusterIP:port).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::io;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::RwLock;
use tracing::{debug, error, info};

use crate::crds::port_mapping::Protocol;

/// A running proxy instance.
struct ProxyHandle {
    local_port: u16,
    target: String,
    protocol: Protocol,
    shutdown: tokio::sync::watch::Sender<bool>,
}

/// Manages TCP/UDP proxy instances for annotated Services.
#[derive(Clone)]
pub struct ProxyManager {
    /// Map of proxy key (e.g., "ns/name/80/TCP") to running proxy handle
    proxies: Arc<RwLock<HashMap<String, ProxyHandle>>>,
    /// The controller's LAN IP (used for PortMapping internalHost)
    pub lan_ip: String,
}

impl ProxyManager {
    pub fn new(lan_ip: String) -> Self {
        Self {
            proxies: Arc::new(RwLock::new(HashMap::new())),
            lan_ip,
        }
    }

    /// Ensure a proxy is running for the given key, targeting the given address.
    /// Returns the local port the proxy is listening on.
    pub async fn ensure_proxy(
        &self,
        key: &str,
        target: &str,
        protocol: &Protocol,
    ) -> Result<u16, String> {
        // Check if already running with same target and protocol
        {
            let proxies = self.proxies.read().await;
            if let Some(handle) = proxies.get(key) {
                if handle.target == target && handle.protocol == *protocol {
                    return Ok(handle.local_port);
                }
                // Target or protocol changed — will stop and restart below
            }
        }

        // Stop existing proxy if target changed
        self.stop_proxy(key).await;

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let key_clone = key.to_string();
        let target_clone = target.to_string();

        let local_port = match protocol {
            Protocol::Tcp => {
                let listener = TcpListener::bind("0.0.0.0:0")
                    .await
                    .map_err(|e| format!("Failed to bind TCP proxy listener: {}", e))?;
                let local_port = listener
                    .local_addr()
                    .map_err(|e| format!("Failed to get local addr: {}", e))?
                    .port();

                tokio::spawn(async move {
                    run_tcp_proxy(listener, &target_clone, shutdown_rx, &key_clone).await;
                });

                local_port
            }
            Protocol::Udp => {
                let socket = UdpSocket::bind("0.0.0.0:0")
                    .await
                    .map_err(|e| format!("Failed to bind UDP proxy socket: {}", e))?;
                let local_port = socket
                    .local_addr()
                    .map_err(|e| format!("Failed to get local addr: {}", e))?
                    .port();

                tokio::spawn(async move {
                    run_udp_proxy(socket, &target_clone, shutdown_rx, &key_clone).await;
                });

                local_port
            }
        };

        info!(
            "Started {} proxy {}: 0.0.0.0:{} -> {}",
            protocol, key, local_port, target
        );

        let handle = ProxyHandle {
            local_port,
            target: target.to_string(),
            protocol: protocol.clone(),
            shutdown: shutdown_tx,
        };

        self.proxies.write().await.insert(key.to_string(), handle);
        Ok(local_port)
    }

    /// Stop a proxy by key.
    pub async fn stop_proxy(&self, key: &str) {
        let mut proxies = self.proxies.write().await;
        if let Some(handle) = proxies.remove(key) {
            let _ = handle.shutdown.send(true);
            info!(
                "Stopped {} proxy {}: port {}",
                handle.protocol, key, handle.local_port
            );
        }
    }

    /// Get the local port for a running proxy, if any.
    pub async fn get_port(&self, key: &str) -> Option<u16> {
        self.proxies.read().await.get(key).map(|h| h.local_port)
    }

    /// Get all active proxy keys.
    pub async fn active_keys(&self) -> Vec<String> {
        self.proxies.read().await.keys().cloned().collect()
    }
}

/// Run a TCP proxy that forwards connections from the listener to the target.
async fn run_tcp_proxy(
    listener: TcpListener,
    target: &str,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    key: &str,
) {
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((inbound, peer)) => {
                        debug!("TCP proxy {}: connection from {}", key, peer);
                        let target = target.to_string();
                        tokio::spawn(async move {
                            if let Err(e) = proxy_tcp_connection(inbound, &target).await {
                                debug!("TCP proxy connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        error!("TCP proxy {}: accept error: {}", key, e);
                        break;
                    }
                }
            }
            _ = shutdown.changed() => {
                debug!("TCP proxy {}: shutdown signal received", key);
                break;
            }
        }
    }
}

/// Forward a single TCP connection bidirectionally.
async fn proxy_tcp_connection(mut inbound: TcpStream, target: &str) -> io::Result<()> {
    let mut outbound = TcpStream::connect(target).await?;
    let (mut ri, mut wi) = inbound.split();
    let (mut ro, mut wo) = outbound.split();

    let client_to_server = io::copy(&mut ri, &mut wo);
    let server_to_client = io::copy(&mut ro, &mut wi);

    tokio::select! {
        r = client_to_server => { r?; }
        r = server_to_client => { r?; }
    }
    Ok(())
}

/// Run a UDP proxy that forwards datagrams between clients and the target.
///
/// UDP is connectionless, so we track client addresses and relay datagrams
/// between each client and the upstream target via per-client sockets.
async fn run_udp_proxy(
    listener: UdpSocket,
    target: &str,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    key: &str,
) {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use tokio::sync::Mutex;

    let listener = Arc::new(listener);

    // Map client address -> upstream socket for relaying responses back
    let clients: Arc<Mutex<HashMap<SocketAddr, Arc<UdpSocket>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let mut buf = vec![0u8; 65535];

    loop {
        tokio::select! {
            result = listener.recv_from(&mut buf) => {
                match result {
                    Ok((len, client_addr)) => {
                        let data = buf[..len].to_vec();
                        let target_addr: SocketAddr = match target.parse() {
                            Ok(addr) => addr,
                            Err(e) => {
                                error!("UDP proxy {}: invalid target '{}': {}", key, target, e);
                                continue;
                            }
                        };

                        let clients = clients.clone();
                        let listener = listener.clone();
                        let key = key.to_string();

                        // Get or create an upstream socket for this client
                        let upstream = {
                            let mut map = clients.lock().await;
                            if let Some(sock) = map.get(&client_addr) {
                                sock.clone()
                            } else {
                                let sock = match UdpSocket::bind("0.0.0.0:0").await {
                                    Ok(s) => Arc::new(s),
                                    Err(e) => {
                                        error!("UDP proxy {}: failed to bind upstream socket: {}", key, e);
                                        continue;
                                    }
                                };
                                debug!("UDP proxy {}: new client {}", key, client_addr);
                                let sock_clone = sock.clone();
                                map.insert(client_addr, sock.clone());

                                // Spawn a task to relay responses back to this client
                                let listener_clone = listener.clone();
                                let clients_clone = clients.clone();
                                let key_clone = key.clone();
                                tokio::spawn(async move {
                                    let mut resp_buf = vec![0u8; 65535];
                                    loop {
                                        match tokio::time::timeout(
                                            std::time::Duration::from_secs(60),
                                            sock_clone.recv_from(&mut resp_buf),
                                        ).await {
                                            Ok(Ok((len, _from))) => {
                                                if let Err(e) = listener_clone.send_to(&resp_buf[..len], client_addr).await {
                                                    debug!("UDP proxy {}: failed to send response to {}: {}", key_clone, client_addr, e);
                                                    break;
                                                }
                                            }
                                            Ok(Err(e)) => {
                                                debug!("UDP proxy {}: upstream recv error: {}", key_clone, e);
                                                break;
                                            }
                                            Err(_) => {
                                                // Timeout — client idle, clean up
                                                debug!("UDP proxy {}: client {} timed out", key_clone, client_addr);
                                                break;
                                            }
                                        }
                                    }
                                    clients_clone.lock().await.remove(&client_addr);
                                });

                                sock
                            }
                        };

                        // Forward the datagram to the target
                        if let Err(e) = upstream.send_to(&data, target_addr).await {
                            debug!("UDP proxy {}: failed to forward to {}: {}", key, target_addr, e);
                        }
                    }
                    Err(e) => {
                        error!("UDP proxy {}: recv error: {}", key, e);
                        break;
                    }
                }
            }
            _ = shutdown.changed() => {
                debug!("UDP proxy {}: shutdown signal received", key);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crds::port_mapping::Protocol;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Start a TCP echo server, return its address.
    async fn tcp_echo_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        stream.write_all(&buf[..n]).await.unwrap();
                    }
                });
            }
        });
        addr
    }

    /// Start a UDP echo server, return its address.
    async fn udp_echo_server() -> String {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((len, src)) => {
                        socket.send_to(&buf[..len], src).await.unwrap();
                    }
                    Err(_) => break,
                }
            }
        });
        addr
    }

    #[tokio::test]
    async fn test_tcp_proxy_round_trip() {
        let echo_addr = tcp_echo_server().await;
        let manager = ProxyManager::new("127.0.0.1".to_string());

        let proxy_port = manager
            .ensure_proxy("test/tcp", &echo_addr, &Protocol::Tcp)
            .await
            .unwrap();

        // Connect through the proxy
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port))
            .await
            .unwrap();

        let payload = b"hello from TCP test";
        stream.write_all(payload).await.unwrap();

        let mut buf = vec![0u8; 64];
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], payload);

        // Verify idempotent — same key returns same port
        let same_port = manager
            .ensure_proxy("test/tcp", &echo_addr, &Protocol::Tcp)
            .await
            .unwrap();
        assert_eq!(proxy_port, same_port);

        manager.stop_proxy("test/tcp").await;
        assert!(manager.get_port("test/tcp").await.is_none());
    }

    #[tokio::test]
    async fn test_udp_proxy_round_trip() {
        let echo_addr = udp_echo_server().await;
        let manager = ProxyManager::new("127.0.0.1".to_string());

        let proxy_port = manager
            .ensure_proxy("test/udp", &echo_addr, &Protocol::Udp)
            .await
            .unwrap();

        // Send a datagram through the proxy
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let payload = b"hello from UDP test";
        client
            .send_to(payload, format!("127.0.0.1:{}", proxy_port))
            .await
            .unwrap();

        let mut buf = vec![0u8; 64];
        let (n, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.recv_from(&mut buf),
        )
        .await
        .expect("UDP response timed out")
        .unwrap();
        assert_eq!(&buf[..n], payload);

        // Verify idempotent
        let same_port = manager
            .ensure_proxy("test/udp", &echo_addr, &Protocol::Udp)
            .await
            .unwrap();
        assert_eq!(proxy_port, same_port);

        manager.stop_proxy("test/udp").await;
        assert!(manager.get_port("test/udp").await.is_none());
    }

    #[tokio::test]
    async fn test_proxy_restarts_on_target_change() {
        let echo1 = tcp_echo_server().await;
        let echo2 = tcp_echo_server().await;
        let manager = ProxyManager::new("127.0.0.1".to_string());

        let port1 = manager
            .ensure_proxy("test/change", &echo1, &Protocol::Tcp)
            .await
            .unwrap();

        // Change target — should get a new port
        let port2 = manager
            .ensure_proxy("test/change", &echo2, &Protocol::Tcp)
            .await
            .unwrap();
        assert_ne!(port1, port2);

        // Verify new proxy works
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port2))
            .await
            .unwrap();
        stream.write_all(b"test").await.unwrap();
        let mut buf = vec![0u8; 16];
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"test");

        manager.stop_proxy("test/change").await;
    }
}
