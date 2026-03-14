use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use kube::{
    api::{Api, Patch, PatchParams},
    runtime::{
        controller::{Action, Controller},
        finalizer::{finalizer, Event as FinalizerEvent},
        watcher::Config,
    },
    Client, ResourceExt,
};
use serde_json::json;
use tracing::{error, info, warn};

use crate::crds::port_mapping::{Condition, PortMapping, PortMappingStatus};
use crate::metrics::Metrics;
use crate::upnp::port_mapping::UpnpClient;

const FINALIZER: &str = "upnp.k8s.io/cleanup";
const DEFAULT_LEASE_SECS: u32 = 3600;
const RENEWAL_BUFFER_SECS: i64 = 30;

pub struct PortMappingContext {
    pub client: Client,
    pub upnp: Arc<UpnpClient>,
    pub metrics: Arc<Metrics>,
}

pub async fn run(ctx: Arc<PortMappingContext>) {
    let api: Api<PortMapping> = Api::all(ctx.client.clone());
    Controller::new(api, Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|result| async move {
            if let Err(e) = result {
                error!("PortMapping reconcile error: {}", e);
            }
        })
        .await;
}

#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    #[error("Kube error: {0}")]
    Kube(#[from] kube::Error),
    #[error("UPnP error: {0}")]
    #[allow(dead_code)]
    Upnp(String),
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for ReconcileError {
    fn from(e: anyhow::Error) -> Self {
        ReconcileError::Other(e.to_string())
    }
}

async fn reconcile(pm: Arc<PortMapping>, ctx: Arc<PortMappingContext>) -> Result<Action, ReconcileError> {
    let ns = pm.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<PortMapping> = Api::namespaced(ctx.client.clone(), &ns);

    finalizer(&api, FINALIZER, pm, |event| async {
        match event {
            FinalizerEvent::Apply(pm) => reconcile_apply(pm, &api, &ctx).await,
            FinalizerEvent::Cleanup(pm) => reconcile_cleanup(pm, &api, &ctx).await,
        }
    })
    .await
    .map_err(|e| ReconcileError::Other(e.to_string()))
}

async fn reconcile_apply(
    pm: Arc<PortMapping>,
    api: &Api<PortMapping>,
    ctx: &Arc<PortMappingContext>,
) -> Result<Action, ReconcileError> {
    let name = pm.name_any();
    let spec = &pm.spec;
    let protocol = spec.protocol.to_string();
    let description = spec
        .description
        .as_deref()
        .unwrap_or(&name);

    // Check if we need to renew (approaching lease expiry)
    if let Some(status) = &pm.status {
        if status.active {
            if let Some(expiry) = status.lease_expiry {
                let now = Utc::now();
                let secs_until_expiry = (expiry - now).num_seconds();
                if secs_until_expiry > RENEWAL_BUFFER_SECS {
                    // Not yet time to renew
                    return Ok(Action::requeue(Duration::from_secs(
                        (secs_until_expiry - RENEWAL_BUFFER_SECS).max(1) as u64,
                    )));
                }
            }
        }
    }

    info!(
        "Adding port mapping: {} {}:{} -> {}:{}",
        name, spec.external_port, protocol, spec.internal_host, spec.internal_port
    );

    match ctx
        .upnp
        .add_port_mapping(
            spec.external_port,
            &spec.internal_host,
            spec.internal_port,
            &protocol,
            description,
            DEFAULT_LEASE_SECS,
        )
        .await
    {
        Ok(lease_duration) => {
            let now = Utc::now();
            let lease_expiry = if lease_duration > 0 {
                Some(now + chrono::Duration::seconds(lease_duration as i64))
            } else {
                None // permanent
            };

            let external_ip = ctx.upnp.get_external_ip().await.ok();

            let status = PortMappingStatus {
                active: true,
                external_ip,
                lease_expiry,
                last_renewal: Some(now),
                conditions: vec![Condition {
                    r#type: "Active".to_string(),
                    status: "True".to_string(),
                    reason: "MappingEstablished".to_string(),
                    message: None,
                    last_transition_time: Some(now),
                }],
            };

            patch_status(api, &name, &status).await?;
            ctx.metrics.port_mapping_renewals.with_label_values(&[&name]).inc();
            ctx.metrics.active_port_mappings.inc();

            let requeue = lease_expiry
                .map(|e| {
                    let secs = (e - Utc::now()).num_seconds() - RENEWAL_BUFFER_SECS;
                    Duration::from_secs(secs.max(10) as u64)
                })
                .unwrap_or(Duration::from_secs(3600));

            Ok(Action::requeue(requeue))
        }
        Err(e) => {
            warn!("Failed to add port mapping {}: {}", name, e);
            ctx.metrics
                .port_mapping_failures
                .with_label_values(&[&name, "add_failed"])
                .inc();
            patch_status(
                api,
                &name,
                &PortMappingStatus {
                    active: false,
                    conditions: vec![Condition {
                        r#type: "Active".to_string(),
                        status: "False".to_string(),
                        reason: "AddFailed".to_string(),
                        message: Some(e.to_string()),
                        last_transition_time: Some(Utc::now()),
                    }],
                    ..Default::default()
                },
            )
            .await?;
            Ok(Action::requeue(Duration::from_secs(60)))
        }
    }
}

async fn reconcile_cleanup(
    pm: Arc<PortMapping>,
    _api: &Api<PortMapping>,
    ctx: &Arc<PortMappingContext>,
) -> Result<Action, ReconcileError> {
    let name = pm.name_any();
    let spec = &pm.spec;
    let protocol = spec.protocol.to_string();

    info!("Deleting port mapping: {} {}:{}", name, spec.external_port, protocol);

    match ctx
        .upnp
        .delete_port_mapping(spec.external_port, &protocol)
        .await
    {
        Ok(()) => {
            ctx.metrics.active_port_mappings.dec();
            Ok(Action::await_change())
        }
        Err(e) => {
            warn!("Failed to delete port mapping {}: {}", name, e);
            // Still allow cleanup to proceed even if UPnP delete fails
            // (router may have already expired the mapping)
            Ok(Action::await_change())
        }
    }
}

async fn patch_status(api: &Api<PortMapping>, name: &str, status: &PortMappingStatus) -> Result<(), ReconcileError> {
    let patch = json!({ "status": status });
    api.patch_status(
        name,
        &PatchParams::apply("upnp-controller"),
        &Patch::Merge(&patch),
    )
    .await?;
    Ok(())
}

fn error_policy(
    _pm: Arc<PortMapping>,
    error: &ReconcileError,
    _ctx: Arc<PortMappingContext>,
) -> Action {
    error!("PortMapping controller error: {}", error);
    Action::requeue(Duration::from_secs(30))
}
