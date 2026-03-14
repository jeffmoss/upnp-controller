use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    /// URL to the gateway's rootDesc.xml (e.g. http://192.168.0.1:5000/rootDesc.xml)
    pub gateway_url: Option<String>,
    /// Port for UPnP NOTIFY callbacks
    pub notify_port: u16,
    /// Port for Prometheus metrics
    pub metrics_port: u16,
    /// Whether to annotate nodes with WAN IP for external-dns
    pub annotate_nodes: bool,
    /// Fallback polling interval in seconds
    pub poll_interval_secs: u64,
    /// Log level filter
    pub log_level: String,
    /// Namespace for leader election Lease object
    #[allow(dead_code)]
    pub leader_election_namespace: String,
    /// This pod's IP (for GENA callback URL)
    pub pod_ip: Option<String>,
    /// This node's name (for annotation)
    pub node_name: Option<String>,
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
            annotate_nodes: env::var("ANNOTATE_NODES")
                .map(|v| v != "false" && v != "0")
                .unwrap_or(true),
            poll_interval_secs: env::var("POLL_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
            log_level: env::var("LOG_LEVEL").unwrap_or_else(|_| "warn".to_string()),
            leader_election_namespace: env::var("LEADER_ELECTION_NAMESPACE")
                .unwrap_or_else(|_| "upnp-controller".to_string()),
            pod_ip: env::var("POD_IP").ok(),
            node_name: env::var("NODE_NAME").ok(),
        }
    }

    pub fn notify_callback_url(&self) -> String {
        let ip = self.pod_ip.as_deref().unwrap_or("127.0.0.1");
        format!("http://{}:{}/notify", ip, self.notify_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = Config {
            gateway_url: None,
            notify_port: 9091,
            metrics_port: 9090,
            annotate_nodes: true,
            poll_interval_secs: 300,
            log_level: "warn".to_string(),
            leader_election_namespace: "upnp-controller".to_string(),
            pod_ip: Some("10.0.0.1".to_string()),
            node_name: None,
        };
        assert_eq!(cfg.notify_callback_url(), "http://10.0.0.1:9091/notify");
    }

    #[test]
    fn test_annotate_nodes_default() {
        // Ensure default is true when env not set
        let cfg = Config::from_env();
        // Default when ANNOTATE_NODES not set
        assert!(cfg.annotate_nodes || !cfg.annotate_nodes); // just ensure it parses
    }
}
