use std::env;
use std::net::UdpSocket;

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct Config {
    /// URL to the gateway's rootDesc.xml (e.g. http://192.168.0.1:5000/rootDesc.xml)
    pub gateway_url: Option<String>,
    /// Port for UPnP NOTIFY callbacks
    pub notify_port: u16,
    /// Port for Prometheus metrics
    pub metrics_port: u16,
    /// Polling interval when GENA is active (backup)
    pub poll_interval_secs: u64,
    /// Polling interval when GENA is unavailable
    pub poll_interval_fast_secs: u64,
    /// Whether GENA subscription is enabled
    pub gena_enabled: bool,
    /// Log level filter
    pub log_level: String,
    /// Namespace for leader election Lease object
    #[allow(dead_code)]
    pub leader_election_namespace: String,
    /// This pod's IP (for GENA callback URL)
    pub pod_ip: Option<String>,
    /// Detected LAN IP (same subnet as gateway)
    pub lan_ip: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            gateway_url: env::var("GATEWAY_URL").ok(),
            notify_port: env::var("NOTIFY_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(9091),
            metrics_port: env::var("METRICS_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(9090),
            poll_interval_secs: env::var("POLL_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600),
            poll_interval_fast_secs: env::var("POLL_INTERVAL_FAST_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            gena_enabled: env::var("GENA_ENABLED")
                .map(|v| v != "false" && v != "0")
                .unwrap_or(true),
            log_level: env::var("LOG_LEVEL").unwrap_or_else(|_| "warn".to_string()),
            leader_election_namespace: env::var("LEADER_ELECTION_NAMESPACE")
                .unwrap_or_else(|_| "upnp-controller".to_string()),
            pod_ip: env::var("POD_IP").ok(),
            lan_ip: None,
        }
    }

    pub fn notify_callback_url(&self) -> String {
        let ip = self.lan_ip.as_deref()
            .or(self.pod_ip.as_deref())
            .unwrap_or("127.0.0.1");
        format!("http://{}:{}/notify", ip, self.notify_port)
    }
}

/// Detect the local IP on the same network as the gateway.
/// Connects a UDP socket to the gateway IP; the OS picks the correct source interface.
pub fn detect_lan_ip(gateway_host: &str) -> Result<String> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect((gateway_host, 1900))?;
    Ok(socket.local_addr()?.ip().to_string())
}

/// Extract the host (without port) from a URL like "http://192.168.0.1:5000/rootDesc.xml"
pub fn parse_host(url: &str) -> Option<String> {
    let after_scheme = url.strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let host_port = after_scheme.split('/').next()?;
    Some(host_port.split(':').next()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            gateway_url: None,
            notify_port: 9091,
            metrics_port: 9090,
            poll_interval_secs: 600,
            poll_interval_fast_secs: 10,
            gena_enabled: true,
            log_level: "warn".to_string(),
            leader_election_namespace: "upnp-controller".to_string(),
            pod_ip: Some("10.0.0.1".to_string()),
            lan_ip: None,
        }
    }

    #[test]
    fn test_notify_callback_uses_pod_ip() {
        let cfg = test_config();
        assert_eq!(cfg.notify_callback_url(), "http://10.0.0.1:9091/notify");
    }

    #[test]
    fn test_notify_callback_prefers_lan_ip() {
        let mut cfg = test_config();
        cfg.lan_ip = Some("192.168.0.102".to_string());
        assert_eq!(cfg.notify_callback_url(), "http://192.168.0.102:9091/notify");
    }

    #[test]
    fn test_notify_callback_fallback() {
        let mut cfg = test_config();
        cfg.pod_ip = None;
        cfg.lan_ip = None;
        assert_eq!(cfg.notify_callback_url(), "http://127.0.0.1:9091/notify");
    }

    #[test]
    fn test_detect_lan_ip() {
        // This should work on any machine with a default route
        let ip = detect_lan_ip("8.8.8.8").unwrap();
        assert!(!ip.is_empty());
        assert_ne!(ip, "0.0.0.0");
    }

    #[test]
    fn test_parse_host() {
        assert_eq!(parse_host("http://192.168.0.1:5000/rootDesc.xml"), Some("192.168.0.1".to_string()));
        assert_eq!(parse_host("http://10.0.0.1/desc.xml"), Some("10.0.0.1".to_string()));
        assert_eq!(parse_host("not-a-url"), None);
    }

}
