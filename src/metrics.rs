use prometheus::{
    CounterVec, Gauge, Registry,
    TextEncoder, Encoder,
};
use std::sync::Arc;
use anyhow::Result;

pub struct Metrics {
    pub active_port_mappings: Gauge,
    pub port_mapping_renewals: CounterVec,
    pub port_mapping_failures: CounterVec,
    pub wan_ip_changes: prometheus::Counter,
    pub gena_subscription_active: Gauge,
    pub gateway_last_seen: Gauge,
    registry: Registry,
}

impl Metrics {
    pub fn new() -> Result<Arc<Self>> {
        let registry = Registry::new();

        let active_port_mappings = Gauge::with_opts(
            prometheus::Opts::new("upnp_active_port_mappings", "Number of currently active PortMappings"),
        )?;
        registry.register(Box::new(active_port_mappings.clone()))?;

        let port_mapping_renewals = CounterVec::new(
            prometheus::Opts::new("upnp_port_mapping_renewals_total", "Successful port mapping renewals"),
            &["name"],
        )?;
        registry.register(Box::new(port_mapping_renewals.clone()))?;

        let port_mapping_failures = CounterVec::new(
            prometheus::Opts::new("upnp_port_mapping_failures_total", "Failed port mapping add/renew attempts"),
            &["name", "reason"],
        )?;
        registry.register(Box::new(port_mapping_failures.clone()))?;

        let wan_ip_changes = prometheus::Counter::with_opts(
            prometheus::Opts::new("upnp_wan_ip_changes_total", "Detected WAN IP changes"),
        )?;
        registry.register(Box::new(wan_ip_changes.clone()))?;

        let gena_subscription_active = Gauge::with_opts(
            prometheus::Opts::new("upnp_gena_subscription_active", "1 if GENA subscription is live, 0 if polling fallback"),
        )?;
        registry.register(Box::new(gena_subscription_active.clone()))?;

        let gateway_last_seen = Gauge::with_opts(
            prometheus::Opts::new("upnp_gateway_last_seen_seconds", "Unix timestamp of last successful router contact"),
        )?;
        registry.register(Box::new(gateway_last_seen.clone()))?;

        Ok(Arc::new(Self {
            active_port_mappings,
            port_mapping_renewals,
            port_mapping_failures,
            wan_ip_changes,
            gena_subscription_active,
            gateway_last_seen,
            registry,
        }))
    }

    pub fn render(&self) -> Result<String> {
        let encoder = TextEncoder::new();
        let mut buffer = Vec::new();
        encoder.encode(&self.registry.gather(), &mut buffer)?;
        Ok(String::from_utf8(buffer)?)
    }
}
