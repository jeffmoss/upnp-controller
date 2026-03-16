use upnp_controller::config;
use upnp_controller::controllers;
use upnp_controller::metrics;
use upnp_controller::upnp;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
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
use tracing::{debug, info, warn};

use config::{Config, detect_lan_ip, parse_host};
use upnp_controller::proxy::ProxyManager;
use metrics::Metrics;
use upnp::{
    discovery::{discover_gateway, ssdp_discover},
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
    let mut cfg = Config::from_env();

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

    // SSDP discovery
    let ssdp_url = ssdp_discover(Duration::from_secs(3)).await;
    if let Some(ref url) = ssdp_url {
        info!("SSDP discovered gateway: {}", url);
    }

    // Resolve gateway URL
    let root_desc_url = match (&cfg.gateway_url, &ssdp_url) {
        (Some(configured), Some(discovered)) => {
            if configured != discovered {
                warn!(
                    "SSDP discovered {} but GATEWAY_URL is {}, using GATEWAY_URL",
                    discovered, configured
                );
            }
            configured.clone()
        }
        (Some(configured), None) => configured.clone(),
        (None, Some(discovered)) => discovered.clone(),
        (None, None) => bail!("No gateway found via SSDP and GATEWAY_URL not set"),
    };

    // Detect LAN IP from gateway host
    if let Some(host) = parse_host(&root_desc_url) {
        match detect_lan_ip(&host) {
            Ok(ip) => {
                info!("Detected LAN IP: {}", ip);
                cfg.lan_ip = Some(ip);
            }
            Err(e) => warn!("Could not detect LAN IP: {}", e),
        }
    }

    // Discover gateway services
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

    // Subscribe to GENA events (if enabled)
    let gena_active = if cfg.gena_enabled {
        let callback_url = cfg.notify_callback_url();
        if let Some(ref event_url) = event_url {
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
                    warn!("GENA subscribe failed: {}; falling back to fast polling", e);
                    metrics.gena_subscription_active.set(0.0);
                    false
                }
            }
        } else {
            warn!("No event URL found; using polling only");
            false
        }
    } else {
        info!("GENA disabled by configuration");
        false
    };

    // Adaptive polling
    let poll_interval = if gena_active {
        cfg.poll_interval_secs
    } else {
        cfg.poll_interval_fast_secs
    };
    info!("Polling interval: {}s (GENA {})", poll_interval, if gena_active { "active" } else { "inactive" });
    {
        let upnp_clone = upnp_client.clone();
        let state_clone = eventing_state.clone();
        let metrics_clone = metrics.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(poll_interval));
            loop {
                interval.tick().await;
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
        });
    }

    // Start controllers
    let lan_ip = cfg.lan_ip.clone().unwrap_or_else(|| "127.0.0.1".to_string());
    let proxy_manager = ProxyManager::new(lan_ip);

    let pm_ctx = Arc::new(controllers::port_mapping_ctrl::PortMappingContext {
        client: client.clone(),
        upnp: upnp_client.clone(),
        metrics: metrics.clone(),
        proxy_manager,
        shutting_down: std::sync::atomic::AtomicBool::new(false),
    });

    let cfg_arc = Arc::new(cfg.clone());
    let gw_ctx = Arc::new(controllers::gateway_ctrl::GatewayContext {
        client: client.clone(),
        eventing_state: eventing_state.clone(),
        metrics: metrics.clone(),
        config: cfg_arc,
        gateway_url: root_desc_url.clone(),
        subscription_id: subscription_id.clone(),
    });

    let dns_ctx = Arc::new(controllers::dns_endpoint_ctrl::DnsEndpointContext {
        client: client.clone(),
        eventing_state: eventing_state.clone(),
    });

    tokio::spawn(controllers::port_mapping_ctrl::run(pm_ctx.clone()));
    tokio::spawn(controllers::port_mapping_ctrl::run_service_watcher(pm_ctx.clone()));
    tokio::spawn(controllers::port_mapping_ctrl::run_pod_watcher(pm_ctx.clone()));
    tokio::spawn(controllers::gateway_ctrl::run(gw_ctx));
    tokio::spawn(controllers::dns_endpoint_ctrl::run(dns_ctx));

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

    // Spawn servers
    tokio::spawn(async move {
        let _ = tokio::try_join!(
            axum::serve(
                tokio::net::TcpListener::bind(&notify_addr).await.unwrap(),
                notify_app,
            ),
            axum::serve(
                tokio::net::TcpListener::bind(&metrics_addr).await.unwrap(),
                metrics_app,
            ),
        );
    });

    // Wait for shutdown signal
    shutdown_signal().await;
    info!("Received shutdown signal, cleaning up...");

    // Tell reconcilers to stop creating/patching resources
    pm_ctx.shutting_down.store(true, std::sync::atomic::Ordering::Relaxed);

    graceful_shutdown(&client, &upnp_client.clone()).await;
    info!("Shutdown complete");

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

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await.ok();
}

/// Delete all PortMapping CRs and wait for finalization to complete.
/// The reconcile loop is still running, so the finalizer fires normally:
/// reconcile_cleanup → DeletePortMapping on router → remove finalizer → k8s GC.
async fn graceful_shutdown(client: &Client, _upnp: &Arc<UpnpClient>) {
    use kube::api::{Api, DeleteParams, ListParams};
    use upnp_controller::crds::port_mapping::PortMapping;

    let api: Api<PortMapping> = Api::all(client.clone());
    let pms = match api.list(&ListParams::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            warn!("Failed to list PortMappings during shutdown: {}", e);
            return;
        }
    };

    info!("Graceful shutdown: deleting {} PortMappings", pms.len());
    for pm in &pms {
        let name = pm.metadata.name.as_deref().unwrap_or("?");
        let ns = pm.metadata.namespace.as_deref().unwrap_or("default");
        let ns_api: Api<PortMapping> = Api::namespaced(client.clone(), ns);
        match ns_api.delete(name, &DeleteParams::default()).await {
            Ok(_) => info!("Shutdown: deleted {}/{}", ns, name),
            Err(e) => warn!("Shutdown: failed to delete {}/{}: {}", ns, name, e),
        }
    }

    // Delete GatewayStatus singleton
    let gs_api: Api<upnp_controller::crds::gateway_status::GatewayStatus> = Api::all(client.clone());
    match gs_api.delete("default", &DeleteParams::default()).await {
        Ok(_) => info!("Shutdown: deleted GatewayStatus/default"),
        Err(e) => warn!("Shutdown: failed to delete GatewayStatus: {}", e),
    }

    // Wait for all resources to be fully gone (finalizers processed by reconcile loops)
    loop {
        let pm_count = api.list(&ListParams::default()).await
            .map(|list| list.items.len()).unwrap_or(0);
        let gs_exists = gs_api.get("default").await.is_ok();

        if pm_count == 0 && !gs_exists {
            info!("Shutdown: all resources cleaned up");
            break;
        }
        debug!("Shutdown: waiting for {} PortMappings, GatewayStatus={}", pm_count, if gs_exists { "exists" } else { "gone" });
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
