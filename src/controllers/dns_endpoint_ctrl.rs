//! Watches DNSEndpoint resources annotated with `upnp-controller.io/managed: "true"`
//! and patches their A record targets with the current WAN IP.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::{StreamExt, TryStreamExt};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::{
    api::{Api, Patch, PatchParams},
    runtime::{
        controller::{Action, Controller},
        watcher,
        watcher::Config,
    },
    Client, ResourceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::upnp::eventing::EventingState;

const MANAGED_ANNOTATION: &str = "upnp-controller.io/managed";
const CRD_NAME: &str = "dnsendpoints.externaldns.k8s.io";

#[derive(Clone, Debug, Deserialize, Serialize, kube::CustomResource, schemars::JsonSchema)]
#[kube(
    group = "externaldns.k8s.io",
    version = "v1alpha1",
    kind = "DNSEndpoint",
    namespaced
)]
#[serde(rename_all = "camelCase")]
pub struct DNSEndpointSpec {
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    #[serde(default)]
    pub dns_name: String,
    #[serde(default)]
    pub record_type: String,
    #[serde(default)]
    pub record_ttl: Option<i64>,
    #[serde(default)]
    pub targets: Vec<String>,
}

pub struct DnsEndpointContext {
    pub client: Client,
    pub eventing_state: Arc<EventingState>,
    pub active: Arc<AtomicBool>,
}

pub async fn run(ctx: Arc<DnsEndpointContext>) {
    loop {
        wait_for_crd(&ctx.client).await;

        ctx.active.store(true, Ordering::Relaxed);
        info!("DNSEndpoint CRD established, starting controller");

        let api: Api<DNSEndpoint> = Api::all(ctx.client.clone());
        Controller::new(api, Config::default())
            .run(reconcile, error_policy, ctx.clone())
            .for_each(|result| async move {
                if let Err(e) = result {
                    error!("DNSEndpoint reconcile error: {}", e);
                }
            })
            .await;

        ctx.active.store(false, Ordering::Relaxed);
        warn!("DNSEndpoint controller stopped, waiting for CRD to reappear...");
    }
}

async fn wait_for_crd(client: &Client) {
    let crd_api: Api<CustomResourceDefinition> = Api::all(client.clone());

    if crd_api.get(CRD_NAME).await.is_ok() {
        return;
    }

    info!("DNSEndpoint CRD not found, watching for it...");
    let watch_config = watcher::Config::default()
        .fields(&format!("metadata.name={}", CRD_NAME));
    let mut stream = watcher::watcher(crd_api, watch_config).boxed();
    while let Ok(Some(event)) = stream.try_next().await {
        if let watcher::Event::Apply(crd) = event {
            if crd.metadata.name.as_deref() == Some(CRD_NAME) {
                return;
            }
        }
    }
}

/// Called from main.rs when the WAN IP changes. Patches all managed DNSEndpoints immediately.
pub async fn update_all_managed(client: &Client, wan_ip: &str) {
    let api: Api<DNSEndpoint> = Api::all(client.clone());
    let endpoints = match api.list(&Default::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            warn!("Failed to list DNSEndpoints for IP update: {}", e);
            return;
        }
    };

    for ep in endpoints {
        let managed = ep
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(MANAGED_ANNOTATION))
            .map(|v| v == "true")
            .unwrap_or(false);
        if !managed {
            continue;
        }

        patch_targets(&ep, client, wan_ip).await;
    }
}

async fn patch_targets(ep: &DNSEndpoint, client: &Client, wan_ip: &str) {
    let name = ep.name_any();
    let ns = ep.namespace().unwrap_or_else(|| "default".to_string());

    let mut patched_spec = ep.spec.clone();
    for endpoint in &mut patched_spec.endpoints {
        if endpoint.record_type == "A" {
            endpoint.targets = vec![wan_ip.to_string()];
        }
    }

    let api: Api<DNSEndpoint> = Api::namespaced(client.clone(), &ns);
    let patch = Patch::Merge(json!({
        "spec": serde_json::to_value(&patched_spec).unwrap_or_default()
    }));
    match api.patch(&name, &PatchParams::default(), &patch).await {
        Ok(_) => info!("Updated DNSEndpoint {}/{}: A targets -> {}", ns, name, wan_ip),
        Err(e) => warn!("Failed to update DNSEndpoint {}/{}: {}", ns, name, e),
    }
}

async fn reconcile(
    ep: Arc<DNSEndpoint>,
    ctx: Arc<DnsEndpointContext>,
) -> Result<Action, kube::Error> {
    let managed = ep
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(MANAGED_ANNOTATION))
        .map(|v| v == "true")
        .unwrap_or(false);

    if !managed {
        return Ok(Action::await_change());
    }

    let wan_ip = ctx.eventing_state.current_external_ip.read().await.clone();
    let wan_ip = match wan_ip {
        Some(ip) => ip,
        None => {
            debug!("No WAN IP yet, requeuing");
            return Ok(Action::requeue(Duration::from_secs(10)));
        }
    };

    patch_targets(&ep, &ctx.client, &wan_ip).await;
    Ok(Action::requeue(Duration::from_secs(300)))
}

fn error_policy(
    _ep: Arc<DNSEndpoint>,
    error: &kube::Error,
    _ctx: Arc<DnsEndpointContext>,
) -> Action {
    error!("DNSEndpoint controller error: {}", error);
    Action::requeue(Duration::from_secs(30))
}
