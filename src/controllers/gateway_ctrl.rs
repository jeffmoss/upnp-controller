use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use kube::{
    api::{Api, ObjectMeta, Patch, PatchParams},
    runtime::{
        controller::{Action, Controller},
        watcher::Config,
    },
    Client,
};
use serde_json::json;
use tracing::{debug, info, warn};

use crate::config::Config as AppConfig;
use crate::crds::gateway_status::{GatewayStatus, GatewayStatusSpec, GatewayStatusStatus, GATEWAY_STATUS_NAME};
use crate::metrics::Metrics;
use crate::upnp::eventing::EventingState;

pub struct GatewayContext {
    pub client: Client,
    pub eventing_state: Arc<EventingState>,
    pub metrics: Arc<Metrics>,
    pub config: Arc<AppConfig>,
    pub gateway_url: String,
    pub subscription_id: Arc<tokio::sync::RwLock<Option<String>>>,
}

pub async fn run(ctx: Arc<GatewayContext>) {
    let api: Api<GatewayStatus> = Api::all(ctx.client.clone());

    // Ensure singleton exists
    ensure_singleton(&api).await;

    Controller::new(api, Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|result| async move {
            if let Err(e) = result {
                tracing::error!("GatewayStatus reconcile error: {}", e);
            }
        })
        .await;
}

async fn ensure_singleton(api: &Api<GatewayStatus>) {
    match api.get_opt(GATEWAY_STATUS_NAME).await {
        Ok(Some(_)) => debug!("GatewayStatus/default already exists"),
        Ok(None) => {
            info!("Creating GatewayStatus/default singleton");
            let gs = GatewayStatus {
                metadata: ObjectMeta {
                    name: Some(GATEWAY_STATUS_NAME.to_string()),
                    ..Default::default()
                },
                spec: GatewayStatusSpec {},
                status: None,
            };
            if let Err(e) = api
                .create(&kube::api::PostParams::default(), &gs)
                .await
            {
                warn!("Failed to create GatewayStatus/default: {}", e);
            }
        }
        Err(e) => warn!("Error checking for GatewayStatus/default: {}", e),
    }
}

pub async fn reconcile(gs: Arc<GatewayStatus>, ctx: Arc<GatewayContext>) -> Result<Action, kube::Error> {
    let api: Api<GatewayStatus> = Api::all(ctx.client.clone());

    // Read current eventing state
    let external_ip = ctx.eventing_state.current_external_ip.read().await.clone();
    let sid = ctx.subscription_id.read().await.clone();

    let now = Utc::now();

    // Only patch status if something meaningful changed (avoid infinite reconcile loop)
    let needs_update = match &gs.status {
        None => true,
        Some(old) => {
            old.external_ip != external_ip
                || old.gateway_url.as_deref() != Some(&ctx.gateway_url)
                || old.subscription_id != sid
                || old.lan_ip != ctx.config.lan_ip
                || old.ready != external_ip.is_some()
        }
    };

    if needs_update {
        let status = GatewayStatusStatus {
            external_ip: external_ip.clone(),
            gateway_url: Some(ctx.gateway_url.clone()),
            subscription_id: sid,
            subscription_expiry: None,
            last_seen: Some(now),
            lan_ip: ctx.config.lan_ip.clone(),
            ready: external_ip.is_some(),
        };

        let patch = json!({
            "apiVersion": "upnp.k8s.io/v1alpha1",
            "kind": "GatewayStatus",
            "status": status
        });
        if let Err(e) = api
            .patch_status(
                GATEWAY_STATUS_NAME,
                &PatchParams::apply("upnp-controller").force(),
                &Patch::Apply(&patch),
            )
            .await
        {
            warn!("Failed to patch GatewayStatus status: {}", e);
        }
    }

    ctx.metrics.gateway_last_seen.set(now.timestamp() as f64);

    Ok(Action::requeue(Duration::from_secs(60)))
}

fn error_policy(
    _gs: Arc<GatewayStatus>,
    error: &kube::Error,
    _ctx: Arc<GatewayContext>,
) -> Action {
    tracing::error!("GatewayStatus controller error: {}", error);
    Action::requeue(Duration::from_secs(30))
}
