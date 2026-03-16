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
        if !is_managed(&ep) {
            continue;
        }
        patch_targets(&ep, client, wan_ip).await;
    }
}

async fn patch_targets(ep: &DNSEndpoint, client: &Client, wan_ip: &str) {
    let name = ep.name_any();
    let ns = ep.namespace().unwrap_or_else(|| "default".to_string());

    let patched_spec = build_patched_spec(&ep.spec, wan_ip);
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
    if !is_managed(&ep) {
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

    if !targets_correct(&ep.spec, &wan_ip) {
        patch_targets(&ep, &ctx.client, &wan_ip).await;
    }
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

/// Check if a DNSEndpoint has the managed annotation.
fn is_managed(ep: &DNSEndpoint) -> bool {
    ep.metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(MANAGED_ANNOTATION))
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Check if all A record targets already match the given IP.
fn targets_correct(spec: &DNSEndpointSpec, wan_ip: &str) -> bool {
    spec.endpoints.iter().all(|e| {
        e.record_type != "A" || e.targets == vec![wan_ip.to_string()]
    })
}

/// Build a patched spec with A record targets set to the given IP.
fn build_patched_spec(spec: &DNSEndpointSpec, wan_ip: &str) -> DNSEndpointSpec {
    let mut patched = spec.clone();
    for endpoint in &mut patched.endpoints {
        if endpoint.record_type == "A" {
            endpoint.targets = vec![wan_ip.to_string()];
        }
    }
    patched
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::api::ObjectMeta;
    use std::collections::BTreeMap;

    fn make_endpoint(dns_name: &str, record_type: &str, targets: Vec<&str>) -> Endpoint {
        Endpoint {
            dns_name: dns_name.to_string(),
            record_type: record_type.to_string(),
            record_ttl: Some(60),
            targets: targets.into_iter().map(String::from).collect(),
        }
    }

    fn make_dns_endpoint(annotations: Option<BTreeMap<String, String>>, endpoints: Vec<Endpoint>) -> DNSEndpoint {
        DNSEndpoint {
            metadata: ObjectMeta {
                name: Some("test".to_string()),
                namespace: Some("default".to_string()),
                annotations,
                ..Default::default()
            },
            spec: DNSEndpointSpec { endpoints },
        }
    }

    #[test]
    fn test_is_managed_with_annotation() {
        let mut annotations = BTreeMap::new();
        annotations.insert(MANAGED_ANNOTATION.to_string(), "true".to_string());
        let ep = make_dns_endpoint(Some(annotations), vec![]);
        assert!(is_managed(&ep));
    }

    #[test]
    fn test_is_managed_without_annotation() {
        let ep = make_dns_endpoint(None, vec![]);
        assert!(!is_managed(&ep));
    }

    #[test]
    fn test_is_managed_wrong_value() {
        let mut annotations = BTreeMap::new();
        annotations.insert(MANAGED_ANNOTATION.to_string(), "false".to_string());
        let ep = make_dns_endpoint(Some(annotations), vec![]);
        assert!(!is_managed(&ep));
    }

    #[test]
    fn test_targets_correct_when_matching() {
        let spec = DNSEndpointSpec {
            endpoints: vec![make_endpoint("home.example.com", "A", vec!["1.2.3.4"])],
        };
        assert!(targets_correct(&spec, "1.2.3.4"));
    }

    #[test]
    fn test_targets_incorrect_when_different() {
        let spec = DNSEndpointSpec {
            endpoints: vec![make_endpoint("home.example.com", "A", vec!["1.2.3.4"])],
        };
        assert!(!targets_correct(&spec, "5.6.7.8"));
    }

    #[test]
    fn test_targets_correct_ignores_non_a_records() {
        let spec = DNSEndpointSpec {
            endpoints: vec![
                make_endpoint("home.example.com", "A", vec!["1.2.3.4"]),
                make_endpoint("home.example.com", "CNAME", vec!["other.example.com"]),
            ],
        };
        assert!(targets_correct(&spec, "1.2.3.4"));
    }

    #[test]
    fn test_targets_correct_empty_targets() {
        let spec = DNSEndpointSpec {
            endpoints: vec![make_endpoint("home.example.com", "A", vec![])],
        };
        assert!(!targets_correct(&spec, "1.2.3.4"));
    }

    #[test]
    fn test_build_patched_spec_updates_a_records_only() {
        let spec = DNSEndpointSpec {
            endpoints: vec![
                make_endpoint("home.example.com", "A", vec!["1.2.3.4"]),
                make_endpoint("home.example.com", "CNAME", vec!["other.example.com"]),
            ],
        };
        let patched = build_patched_spec(&spec, "5.6.7.8");
        assert_eq!(patched.endpoints[0].targets, vec!["5.6.7.8"]);
        assert_eq!(patched.endpoints[1].targets, vec!["other.example.com"]);
    }

    #[test]
    fn test_build_patched_spec_multiple_a_records() {
        let spec = DNSEndpointSpec {
            endpoints: vec![
                make_endpoint("a.example.com", "A", vec!["1.1.1.1"]),
                make_endpoint("b.example.com", "A", vec!["2.2.2.2"]),
            ],
        };
        let patched = build_patched_spec(&spec, "9.9.9.9");
        assert_eq!(patched.endpoints[0].targets, vec!["9.9.9.9"]);
        assert_eq!(patched.endpoints[1].targets, vec!["9.9.9.9"]);
    }
}
