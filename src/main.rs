mod config;
mod crds;
mod controllers;
mod metrics;
mod node;
mod upnp;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use chrono::Utc;
use kube::Client;
use tokio::sync::RwLock;
use tracing::{info, warn};

use config::Config;
use metrics::Metrics;
use upnp::{
    discovery::discover_gateway,
    eventing::{parse_notify_body, subscribe, run_renewal_loop, EventingState},
    port_mapping::UpnpClient,
};

#[derive(Clone)]
struct AppState {
    eventing_state: Arc<EventingState>,
    metrics: Arc<Metrics>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::from_env();

    // Init tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cfg.log_level.parse().unwrap_or_default()),
        )
        .json()
        .init();

    info!("upnp-controller starting");

    let metrics = Metrics::new().context("Failed to init metrics")?;
    let eventing_state = EventingState::new();
    let subscription_id: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));

    // Connect to Kubernetes
    let client = Client::try_default()
        .await
        .context("Failed to connect to Kubernetes")?;
    info!("Connected to Kubernetes");

    // Discover gateway services
    let root_desc_url = cfg
        .gateway_url
        .clone()
        .unwrap_or_else(|| "http://192.168.0.1:5000/rootDesc.xml".to_string());

    let gateway_services = discover_gateway(&root_desc_url)
        .await
        .context("Failed to discover gateway services")?;

    let control_url = gateway_services
        .control_url()
        .context("No WANIPConnection or WANPPPConnection found in rootDesc.xml")?
        .to_string();

    let event_url = gateway_services
        .event_url()
        .map(|s| s.to_string());

    info!("Gateway control URL: {}", control_url);

    let upnp_client = Arc::new(UpnpClient::new(control_url));

    // Get initial external IP
    match upnp_client.get_external_ip().await {
        Ok(ip) => {
            info!("Current external IP: {}", ip);
            *eventing_state.current_external_ip.write().await = Some(ip);
            metrics.gateway_last_seen.set(Utc::now().timestamp() as f64);
        }
        Err(e) => warn!("Could not get initial external IP: {}", e),
    }

    // Subscribe to GENA events
    let callback_url = cfg.notify_callback_url();
    let gena_active = if let Some(ref event_url) = event_url {
        match subscribe(event_url, &callback_url, 1800).await {
            Ok(sid) => {
                info!("GENA subscribed: SID={}", sid);
                *subscription_id.write().await = Some(sid.clone());
                metrics.gena_subscription_active.set(1.0);

                // Start renewal loop
                let state_clone = eventing_state.clone();
                let event_url_clone = event_url.clone();
                tokio::spawn(run_renewal_loop(event_url_clone, state_clone, 1800));

                true
            }
            Err(e) => {
                warn!("GENA subscribe failed: {}; falling back to polling", e);
                metrics.gena_subscription_active.set(0.0);
                false
            }
        }
    } else {
        warn!("No event URL found; using polling only");
        false
    };

    // Start polling fallback
    {
        let upnp_clone = upnp_client.clone();
        let state_clone = eventing_state.clone();
        let metrics_clone = metrics.clone();
        let poll_interval = cfg.poll_interval_secs;
        let gena_was_active = gena_active;

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(poll_interval));
            loop {
                interval.tick().await;

                // Check if GENA has been silent
                let should_poll = {
                    let last_notify = state_clone.last_notify.read().await;
                    match *last_notify {
                        None => true,
                        Some(t) => {
                            t.elapsed() > Duration::from_secs(600) // 10 minutes
                        }
                    }
                };

                if should_poll || !gena_was_active {
                    match upnp_clone.get_external_ip().await {
                        Ok(ip) => {
                            let mut current = state_clone.current_external_ip.write().await;
                            if current.as_deref() != Some(&ip) {
                                info!("WAN IP changed (poll): {} -> {}", current.as_deref().unwrap_or("none"), ip);
                                metrics_clone.wan_ip_changes.inc();
                            }
                            *current = Some(ip);
                            metrics_clone.gateway_last_seen.set(Utc::now().timestamp() as f64);
                        }
                        Err(e) => warn!("Poll GetExternalIPAddress failed: {}", e),
                    }
                }
            }
        });
    }

    // Start controllers
    let pm_ctx = Arc::new(controllers::port_mapping_ctrl::PortMappingContext {
        client: client.clone(),
        upnp: upnp_client.clone(),
        metrics: metrics.clone(),
    });

    let cfg_arc = Arc::new(cfg.clone());
    let gw_ctx = Arc::new(controllers::gateway_ctrl::GatewayContext {
        client: client.clone(),
        eventing_state: eventing_state.clone(),
        metrics: metrics.clone(),
        config: cfg_arc,
        node_name: cfg.node_name.clone(),
        gateway_url: root_desc_url.clone(),
        subscription_id: subscription_id.clone(),
    });

    tokio::spawn(controllers::port_mapping_ctrl::run(pm_ctx));
    tokio::spawn(controllers::gateway_ctrl::run(gw_ctx));

    // Start axum server
    let state = AppState {
        eventing_state: eventing_state.clone(),
        metrics: metrics.clone(),
    };

    let notify_port = cfg.notify_port;
    let metrics_port = cfg.metrics_port;

    let notify_app = Router::new()
        .route("/notify", post(handle_notify))
        .with_state(state.clone());

    let metrics_app = Router::new()
        .route("/metrics", get(handle_metrics))
        .with_state(state.clone());

    let notify_addr = format!("0.0.0.0:{}", notify_port);
    let metrics_addr = format!("0.0.0.0:{}", metrics_port);

    info!("Starting NOTIFY server on {}", notify_addr);
    info!("Starting metrics server on {}", metrics_addr);

    tokio::try_join!(
        axum::serve(
            tokio::net::TcpListener::bind(&notify_addr).await?,
            notify_app,
        ),
        axum::serve(
            tokio::net::TcpListener::bind(&metrics_addr).await?,
            metrics_app,
        ),
    )?;

    Ok(())
}

async fn handle_notify(
    State(state): State<AppState>,
    body: String,
) -> impl IntoResponse {
    if let Some(ip) = parse_notify_body(&body) {
        let mut current = state.eventing_state.current_external_ip.write().await;
        if current.as_deref() != Some(&ip) {
            info!("WAN IP changed (GENA): {} -> {}", current.as_deref().unwrap_or("none"), ip);
            state.metrics.wan_ip_changes.inc();
        }
        *current = Some(ip);
        *state.eventing_state.last_notify.write().await = Some(tokio::time::Instant::now());
        state.metrics.gateway_last_seen.set(Utc::now().timestamp() as f64);
    }
    StatusCode::OK
}

async fn handle_metrics(State(state): State<AppState>) -> impl IntoResponse {
    match state.metrics.render() {
        Ok(body) => (StatusCode::OK, body),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
