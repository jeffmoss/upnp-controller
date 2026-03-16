use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{ContainerPort, Pod, Service};
use kube::{
    api::{Api, DeleteParams, ListParams, Patch, PatchParams},
    runtime::{
        controller::{Action, Controller},
        finalizer::{finalizer, Event as FinalizerEvent},
        watcher::Config,
    },
    Client, ResourceExt,
};
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::crds::port_mapping::{Condition, PortMapping, PortMappingStatus, Protocol};
use crate::metrics::Metrics;
use crate::upnp::port_mapping::UpnpClient;

const FINALIZER: &str = "upnp-controller.io/cleanup";
const DEFAULT_LEASE_SECS: u32 = 3600;
const RENEWAL_BUFFER_SECS: i64 = 30;

const PORT_FORWARD_ANNOTATION: &str = "upnp-controller.io/port-forward";
const MANAGED_BY_LABEL: &str = "upnp-controller.io/managed-by";
const MANAGED_BY_VALUE: &str = "service-controller";

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

pub async fn reconcile(pm: Arc<PortMapping>, ctx: Arc<PortMappingContext>) -> Result<Action, ReconcileError> {
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
    let patch = json!({
        "apiVersion": "upnp-controller.io/v1alpha1",
        "kind": "PortMapping",
        "status": status
    });
    api.patch_status(
        name,
        &PatchParams::apply("upnp-controller").force(),
        &Patch::Apply(&patch),
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

// ---------------------------------------------------------------------------
// Annotation watchers (Service + Pod)
// ---------------------------------------------------------------------------

/// Run the Service annotation watcher. Watches LoadBalancer Services for the
/// `upnp-controller.io/port-forward` annotation and creates/deletes PortMapping CRs.
pub async fn run_service_watcher(ctx: Arc<PortMappingContext>) {
    let services: Api<Service> = Api::all(ctx.client.clone());
    Controller::new(services, Config::default())
        .run(reconcile_service, service_error_policy, ctx)
        .for_each(|result| async move {
            if let Err(e) = result {
                error!("Service watcher reconcile error: {}", e);
            }
        })
        .await;
}

/// Run the Pod annotation watcher. Watches Pods for the
/// `upnp-controller.io/port-forward` annotation and creates/deletes PortMapping CRs.
pub async fn run_pod_watcher(ctx: Arc<PortMappingContext>) {
    let pods: Api<Pod> = Api::all(ctx.client.clone());
    Controller::new(pods, Config::default())
        .run(reconcile_pod, pod_error_policy, ctx)
        .for_each(|result| async move {
            if let Err(e) = result {
                error!("Pod watcher reconcile error: {}", e);
            }
        })
        .await;
}

// --- Shared helpers ---

/// Find owned PortMappings for a given owner UID.
async fn list_owned_port_mappings(
    pm_api: &Api<PortMapping>,
    owner_uid: &str,
) -> Result<Vec<PortMapping>, ReconcileError> {
    let existing = pm_api
        .list(&ListParams::default().labels(&format!("{}={}", MANAGED_BY_LABEL, MANAGED_BY_VALUE)))
        .await?;
    Ok(existing
        .items
        .into_iter()
        .filter(|pm| {
            pm.metadata
                .owner_references
                .as_ref()
                .is_some_and(|refs| refs.iter().any(|r| r.uid == owner_uid))
        })
        .collect())
}

/// Delete all owned PortMappings (used when annotation is removed).
async fn cleanup_owned_port_mappings(pm_api: &Api<PortMapping>, owned: &[PortMapping]) {
    for pm in owned {
        let name = pm.name_any();
        debug!("Removing managed PortMapping {} (annotation removed)", name);
        if let Err(e) = pm_api.delete(&name, &DeleteParams::default()).await {
            warn!("Failed to delete managed PortMapping {}: {}", name, e);
        }
    }
}

/// Apply desired PortMappings and delete stale ones.
#[allow(clippy::too_many_arguments)]
async fn apply_port_mappings(
    pm_api: &Api<PortMapping>,
    ns: &str,
    owner_name: &str,
    owner_uid: &str,
    owner_kind: &str,
    owner_api_version: &str,
    ip: &str,
    desired: &[(u16, u16, Protocol)],
    owned: &[PortMapping],
) -> Result<Action, ReconcileError> {
    let desired_names: Vec<String> = desired
        .iter()
        .map(|(ext, _int, proto)| pm_name_for_owner(ns, owner_name, *ext, proto))
        .collect();

    for (i, (external_port, internal_port, protocol)) in desired.iter().enumerate() {
        let name = &desired_names[i];

        let pm_manifest = json!({
            "apiVersion": "upnp-controller.io/v1alpha1",
            "kind": "PortMapping",
            "metadata": {
                "name": name,
                "namespace": ns,
                "labels": {
                    MANAGED_BY_LABEL: MANAGED_BY_VALUE
                },
                "ownerReferences": [{
                    "apiVersion": owner_api_version,
                    "kind": owner_kind,
                    "name": owner_name,
                    "uid": owner_uid,
                    "controller": true,
                    "blockOwnerDeletion": true
                }]
            },
            "spec": {
                "externalPort": external_port,
                "internalHost": ip,
                "internalPort": internal_port,
                "protocol": protocol.to_string(),
                "description": format!("auto:{}/{}", ns, owner_name)
            }
        });

        pm_api
            .patch(
                name,
                &PatchParams::apply("upnp-service-controller").force(),
                &Patch::Apply(&pm_manifest),
            )
            .await?;

        info!(
            "Managed PortMapping {}: {}:{} -> {}:{}",
            name, external_port, protocol, ip, internal_port
        );
    }

    // Delete stale PortMappings
    for pm in owned {
        let name = pm.name_any();
        if !desired_names.contains(&name) {
            info!("Removing stale managed PortMapping {}", name);
            if let Err(e) = pm_api.delete(&name, &DeleteParams::default()).await {
                warn!("Failed to delete stale PortMapping {}: {}", name, e);
            }
        }
    }

    Ok(Action::requeue(Duration::from_secs(60)))
}

// --- Service reconciler ---

async fn reconcile_service(
    svc: Arc<Service>,
    ctx: Arc<PortMappingContext>,
) -> Result<Action, ReconcileError> {
    let svc_name = svc.name_any();
    let svc_ns = svc.namespace().unwrap_or_else(|| "default".to_string());
    let svc_uid = svc.metadata.uid.as_deref().unwrap_or("");

    let annotation_value = svc
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(PORT_FORWARD_ANNOTATION));

    let pm_api: Api<PortMapping> = Api::namespaced(ctx.client.clone(), &svc_ns);
    let owned = list_owned_port_mappings(&pm_api, svc_uid).await?;

    let annotation_value = match annotation_value {
        Some(v) => v,
        None => {
            cleanup_owned_port_mappings(&pm_api, &owned).await;
            return Ok(Action::await_change());
        }
    };

    // Only handle LoadBalancer Services
    let is_lb = svc
        .spec
        .as_ref()
        .map(|s| s.type_.as_deref() == Some("LoadBalancer"))
        .unwrap_or(false);
    if !is_lb {
        debug!("Service {}/{} is not LoadBalancer, ignoring annotation", svc_ns, svc_name);
        return Ok(Action::await_change());
    }

    // Read the LB IP
    let lb_ip = svc
        .status
        .as_ref()
        .and_then(|s| s.load_balancer.as_ref())
        .and_then(|lb| lb.ingress.as_ref())
        .and_then(|ingress| ingress.first())
        .and_then(|entry| entry.ip.as_deref());

    let lb_ip = match lb_ip {
        Some(ip) => ip.to_string(),
        None => {
            debug!("Service {}/{} has no LB IP yet, requeuing", svc_ns, svc_name);
            return Ok(Action::requeue(Duration::from_secs(5)));
        }
    };

    let service_ports = svc.spec.as_ref().map(|s| s.ports.as_deref().unwrap_or(&[])).unwrap_or(&[]);
    let desired = parse_service_annotation(annotation_value, service_ports);

    if desired.is_empty() {
        warn!("Service {}/{}: annotation '{}' matched no ports", svc_ns, svc_name, annotation_value);
        return Ok(Action::await_change());
    }

    apply_port_mappings(&pm_api, &svc_ns, &svc_name, svc_uid, "Service", "v1", &lb_ip, &desired, &owned).await
}

fn service_error_policy(
    _svc: Arc<Service>,
    error: &ReconcileError,
    _ctx: Arc<PortMappingContext>,
) -> Action {
    error!("Service watcher error: {}", error);
    Action::requeue(Duration::from_secs(30))
}

// --- Pod reconciler ---

async fn reconcile_pod(
    pod: Arc<Pod>,
    ctx: Arc<PortMappingContext>,
) -> Result<Action, ReconcileError> {
    let pod_name = pod.name_any();
    let pod_ns = pod.namespace().unwrap_or_else(|| "default".to_string());
    let pod_uid = pod.metadata.uid.as_deref().unwrap_or("");

    let annotation_value = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(PORT_FORWARD_ANNOTATION));

    let pm_api: Api<PortMapping> = Api::namespaced(ctx.client.clone(), &pod_ns);
    let owned = list_owned_port_mappings(&pm_api, pod_uid).await?;

    let annotation_value = match annotation_value {
        Some(v) => v,
        None => {
            cleanup_owned_port_mappings(&pm_api, &owned).await;
            return Ok(Action::await_change());
        }
    };

    // Read the Pod IP
    let pod_ip = pod
        .status
        .as_ref()
        .and_then(|s| s.pod_ip.as_deref());

    let pod_ip = match pod_ip {
        Some(ip) => ip.to_string(),
        None => {
            debug!("Pod {}/{} has no IP yet, requeuing", pod_ns, pod_name);
            return Ok(Action::requeue(Duration::from_secs(5)));
        }
    };

    // Collect container ports for "true" mode
    let container_ports: Vec<ContainerPort> = pod
        .spec
        .as_ref()
        .map(|s| {
            s.containers
                .iter()
                .flat_map(|c| c.ports.as_deref().unwrap_or(&[]).iter().cloned())
                .collect()
        })
        .unwrap_or_default();

    let desired = parse_pod_annotation(annotation_value, &container_ports);

    if desired.is_empty() {
        warn!("Pod {}/{}: annotation '{}' matched no ports", pod_ns, pod_name, annotation_value);
        return Ok(Action::await_change());
    }

    apply_port_mappings(&pm_api, &pod_ns, &pod_name, pod_uid, "Pod", "v1", &pod_ip, &desired, &owned).await
}

fn pod_error_policy(
    _pod: Arc<Pod>,
    error: &ReconcileError,
    _ctx: Arc<PortMappingContext>,
) -> Action {
    error!("Pod watcher error: {}", error);
    Action::requeue(Duration::from_secs(30))
}

// --- Annotation parsing ---

/// Generate a deterministic PortMapping name from an owner object.
fn pm_name_for_owner(ns: &str, name: &str, external_port: u16, protocol: &Protocol) -> String {
    let proto = match protocol {
        Protocol::Tcp => "tcp",
        Protocol::Udp => "udp",
    };
    format!("{}-{}-{}-{}", ns, name, external_port, proto)
}

/// Parse annotation for a Service. Uses ServicePort list for "true" mode.
fn parse_service_annotation(
    value: &str,
    service_ports: &[k8s_openapi::api::core::v1::ServicePort],
) -> Vec<(u16, u16, Protocol)> {
    if value == "true" {
        return service_ports
            .iter()
            .map(|sp| {
                let port = sp.port as u16;
                let protocol = parse_k8s_protocol(sp.protocol.as_deref());
                (port, port, protocol)
            })
            .collect();
    }
    parse_port_list(value, |port| {
        service_ports
            .iter()
            .find(|sp| sp.port as u16 == port)
            .map(|sp| parse_k8s_protocol(sp.protocol.as_deref()))
    })
}

/// Parse annotation for a Pod. Uses ContainerPort list for "true" mode.
fn parse_pod_annotation(
    value: &str,
    container_ports: &[ContainerPort],
) -> Vec<(u16, u16, Protocol)> {
    if value == "true" {
        return container_ports
            .iter()
            .map(|cp| {
                let port = cp.container_port as u16;
                let protocol = parse_k8s_protocol(cp.protocol.as_deref());
                (port, port, protocol)
            })
            .collect();
    }
    parse_port_list(value, |port| {
        container_ports
            .iter()
            .find(|cp| cp.container_port as u16 == port)
            .map(|cp| parse_k8s_protocol(cp.protocol.as_deref()))
    })
}

/// Parse a comma-separated port list: "443,80" or "8080:80,8443:443".
/// The `lookup_protocol` closure finds the protocol for a given service/container port.
fn parse_port_list(
    value: &str,
    lookup_protocol: impl Fn(u16) -> Option<Protocol>,
) -> Vec<(u16, u16, Protocol)> {
    let mut result = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((ext, svc)) = part.split_once(':') {
            if let (Ok(ext_port), Ok(svc_port)) = (ext.trim().parse::<u16>(), svc.trim().parse::<u16>()) {
                let protocol = lookup_protocol(svc_port).unwrap_or(Protocol::Tcp);
                result.push((ext_port, svc_port, protocol));
            }
        } else if let Ok(port) = part.parse::<u16>() {
            // Only include if the port exists on the object
            if let Some(protocol) = lookup_protocol(port) {
                result.push((port, port, protocol));
            }
        }
    }
    result
}

fn parse_k8s_protocol(proto: Option<&str>) -> Protocol {
    match proto {
        Some("UDP") => Protocol::Udp,
        _ => Protocol::Tcp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{ContainerPort, ServicePort};

    fn test_service_ports() -> Vec<ServicePort> {
        vec![
            ServicePort { port: 80, protocol: Some("TCP".to_string()), ..Default::default() },
            ServicePort { port: 443, protocol: Some("TCP".to_string()), ..Default::default() },
            ServicePort { port: 53, protocol: Some("UDP".to_string()), ..Default::default() },
        ]
    }

    fn test_container_ports() -> Vec<ContainerPort> {
        vec![
            ContainerPort { container_port: 8080, protocol: Some("TCP".to_string()), ..Default::default() },
            ContainerPort { container_port: 9090, protocol: Some("TCP".to_string()), ..Default::default() },
        ]
    }

    #[test]
    fn test_parse_service_annotation_true() {
        let ports = test_service_ports();
        let result = parse_service_annotation("true", &ports);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], (80, 80, Protocol::Tcp));
        assert_eq!(result[1], (443, 443, Protocol::Tcp));
        assert_eq!(result[2], (53, 53, Protocol::Udp));
    }

    #[test]
    fn test_parse_service_annotation_selective() {
        let ports = test_service_ports();
        let result = parse_service_annotation("443,80", &ports);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], (443, 443, Protocol::Tcp));
        assert_eq!(result[1], (80, 80, Protocol::Tcp));
    }

    #[test]
    fn test_parse_service_annotation_remap() {
        let ports = test_service_ports();
        let result = parse_service_annotation("8080:80,8443:443", &ports);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], (8080, 80, Protocol::Tcp));
        assert_eq!(result[1], (8443, 443, Protocol::Tcp));
    }

    #[test]
    fn test_parse_service_annotation_no_match() {
        let ports = test_service_ports();
        let result = parse_service_annotation("9999", &ports);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_parse_pod_annotation_true() {
        let ports = test_container_ports();
        let result = parse_pod_annotation("true", &ports);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], (8080, 8080, Protocol::Tcp));
        assert_eq!(result[1], (9090, 9090, Protocol::Tcp));
    }

    #[test]
    fn test_parse_pod_annotation_selective() {
        let ports = test_container_ports();
        let result = parse_pod_annotation("8080", &ports);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], (8080, 8080, Protocol::Tcp));
    }

    #[test]
    fn test_parse_pod_annotation_remap() {
        let ports = test_container_ports();
        let result = parse_pod_annotation("80:8080", &ports);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], (80, 8080, Protocol::Tcp));
    }

    #[test]
    fn test_pm_name_for_owner() {
        assert_eq!(
            pm_name_for_owner("default", "traefik", 443, &Protocol::Tcp),
            "default-traefik-443-tcp"
        );
    }
}
