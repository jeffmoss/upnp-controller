use chrono::{DateTime, Utc};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayStatusStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_expiry: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
    #[serde(default)]
    pub ready: bool,
}

/// GatewayStatus is a cluster-scoped singleton CRD tracking the router's WAN IP and UPnP subscription state
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "upnp.k8s.io",
    version = "v1alpha1",
    kind = "GatewayStatus",
    status = "GatewayStatusStatus",
    printcolumn = r#"{"name":"External IP","type":"string","jsonPath":".status.externalIP"}"#,
    printcolumn = r#"{"name":"Ready","type":"boolean","jsonPath":".status.ready"}"#,
    printcolumn = r#"{"name":"Last Seen","type":"date","jsonPath":".status.lastSeen"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct GatewayStatusSpec {}

pub const GATEWAY_STATUS_NAME: &str = "default";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gateway_status_default() {
        let status = GatewayStatusStatus::default();
        assert!(!status.ready);
        assert!(status.external_ip.is_none());
        assert!(status.subscription_id.is_none());
    }
}
