//! Watches DNSEndpoint resources annotated with `upnp-controller.io/managed: "true"`
//! and patches their targets with the current WAN IP from GatewayStatus.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::{
    api::{Api, Patch, PatchParams},
    runtime::{
        controller::{Action, Controller},
        watcher::Config,
    },
    Client, ResourceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, error, info};

use crate::upnp::eventing::EventingState;

const MANAGED_ANNOTATION: &str = "upnp-controller.io/managed";

/// Minimal representation of externaldns.k8s.io/v1alpha1 DNSEndpoint.
/// We only need metadata + spec.endpoints[].targets for patching.
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
}

pub async fn run(ctx: Arc<DnsEndpointContext>) {
    wait_for_crd(&ctx.client).await;

    info!("DNSEndpoint CRD established, starting DNS endpoint controller");
    let api: Api<DNSEndpoint> = Api::all(ctx.client.clone());
    Controller::new(api, Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|result| async move {
            if let Err(e) = result {
                error!("DNSEndpoint reconcile error: {}", e);
            }
        })
        .await;
}

/// Watch for the DNSEndpoint CRD to be established.
async fn wait_for_crd(client: &Client) {
    use futures::TryStreamExt;
    use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
    use kube::{api::Api, runtime::watcher};

    let crd_api: Api<CustomResourceDefinition> = Api::all(client.clone());
    let crd_name = "dnsendpoints.externaldns.k8s.io";

    // Fast path
    if crd_api.get(crd_name).await.is_ok() {
        return;
    }

    info!("DNSEndpoint CRD not found, watching for it...");
    let watch_config = watcher::Config::default()
        .fields(&format!("metadata.name={}", crd_name));
    let mut stream = watcher::watcher(crd_api, watch_config).boxed();
    while let Ok(Some(event)) = stream.try_next().await {
        if let watcher::Event::Apply(crd) = event {
            if crd.metadata.name.as_deref() == Some(crd_name) {
                return;
            }
        }
    }
}

async fn reconcile(
    ep: Arc<DNSEndpoint>,
    ctx: Arc<DnsEndpointContext>,
) -> Result<Action, kube::Error> {
    let name = ep.name_any();
    let ns = ep.namespace().unwrap_or_else(|| "default".to_string());

    // Only manage DNSEndpoints with our annotation
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

    // Get current WAN IP
    let wan_ip = ctx.eventing_state.current_external_ip.read().await.clone();
    let wan_ip = match wan_ip {
        Some(ip) => ip,
        None => {
            debug!("No WAN IP yet, requeuing DNSEndpoint {}/{}", ns, name);
            return Ok(Action::requeue(Duration::from_secs(10)));
        }
    };

    // Check if targets already match
    let already_correct = ep.spec.endpoints.iter().all(|e| {
        e.record_type != "A" || (e.targets.len() == 1 && e.targets[0] == wan_ip)
    });

    if already_correct {
        return Ok(Action::requeue(Duration::from_secs(60)));
    }

    // Patch all A record endpoints with the current WAN IP
    let updated_endpoints: Vec<serde_json::Value> = ep
        .spec
        .endpoints
        .iter()
        .map(|e| {
            if e.record_type == "A" {
                json!({
                    "dnsName": e.dns_name,
                    "recordType": "A",
                    "recordTTL": e.record_ttl,
                    "targets": [wan_ip]
                })
            } else {
                serde_json::to_value(e).unwrap_or_default()
            }
        })
        .collect();

    let patch = json!({
        "apiVersion": "externaldns.k8s.io/v1alpha1",
        "kind": "DNSEndpoint",
        "metadata": { "name": name, "namespace": ns },
        "spec": { "endpoints": updated_endpoints }
    });

    let api: Api<DNSEndpoint> = Api::namespaced(ctx.client.clone(), &ns);
    api.patch(
        &name,
        &PatchParams::apply("upnp-controller").force(),
        &Patch::Apply(&patch),
    )
    .await?;

    info!(
        "Updated DNSEndpoint {}/{}: set A record targets to {}",
        ns, name, wan_ip
    );

    Ok(Action::requeue(Duration::from_secs(60)))
}

fn error_policy(
    _ep: Arc<DNSEndpoint>,
    error: &kube::Error,
    _ctx: Arc<DnsEndpointContext>,
) -> Action {
    error!("DNSEndpoint controller error: {}", error);
    Action::requeue(Duration::from_secs(30))
}
