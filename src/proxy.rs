//! TCP proxy for forwarding external traffic to cluster services.
//!
//! Each proxy listens on a local port (on the controller's node) and forwards
//! connections to a cluster-internal target (ClusterIP:port).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::io;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tracing::{debug, error, info};

/// A running TCP proxy instance.
struct ProxyHandle {
    local_port: u16,
    target: String,
    shutdown: tokio::sync::watch::Sender<bool>,
}

/// Manages TCP proxy instances for annotated Services.
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
    pub async fn ensure_proxy(&self, key: &str, target: &str) -> Result<u16, String> {
        // Check if already running with same target
        {
            let proxies = self.proxies.read().await;
            if let Some(handle) = proxies.get(key) {
                if handle.target == target {
                    return Ok(handle.local_port);
                }
                // Target changed — will stop and restart below
            }
        }

        // Stop existing proxy if target changed
        self.stop_proxy(key).await;

        // Start new proxy on an ephemeral port
        let listener = TcpListener::bind("0.0.0.0:0")
            .await
            .map_err(|e| format!("Failed to bind proxy listener: {}", e))?;
        let local_port = listener
            .local_addr()
            .map_err(|e| format!("Failed to get local addr: {}", e))?
            .port();

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let target_clone = target.to_string();
        let key_clone = key.to_string();

        tokio::spawn(async move {
            run_proxy(listener, &target_clone, shutdown_rx, &key_clone).await;
        });

        info!("Started proxy {}: 0.0.0.0:{} -> {}", key, local_port, target);

        let handle = ProxyHandle {
            local_port,
            target: target.to_string(),
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
            info!("Stopped proxy {}: port {}", key, handle.local_port);
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
async fn run_proxy(
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
                        debug!("Proxy {}: connection from {}", key, peer);
                        let target = target.to_string();
                        tokio::spawn(async move {
                            if let Err(e) = proxy_connection(inbound, &target).await {
                                debug!("Proxy connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        error!("Proxy {}: accept error: {}", key, e);
                        break;
                    }
                }
            }
            _ = shutdown.changed() => {
                debug!("Proxy {}: shutdown signal received", key);
                break;
            }
        }
    }
}

/// Forward a single TCP connection bidirectionally.
async fn proxy_connection(mut inbound: TcpStream, target: &str) -> io::Result<()> {
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
