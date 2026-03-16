use chrono::{DateTime, Utc};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Protocol {
    Tcp,
    Udp,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::Tcp => write!(f, "TCP"),
            Protocol::Udp => write!(f, "UDP"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct PortMappingStatus {
    #[serde(default)]
    pub active: bool,
    #[serde(rename = "externalIP", skip_serializing_if = "Option::is_none")]
    pub external_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_expiry: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_renewal: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    pub r#type: String,
    pub status: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<DateTime<Utc>>,
}

/// PortMapping is a CRD that represents a UPnP port mapping on the router
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "upnp-controller.io",
    version = "v1alpha1",
    kind = "PortMapping",
    namespaced,
    status = "PortMappingStatus",
    shortname = "pm",
    printcolumn = r#"{"name":"External Port","type":"integer","jsonPath":".spec.externalPort"}"#,
    printcolumn = r#"{"name":"Internal Host","type":"string","jsonPath":".spec.internalHost"}"#,
    printcolumn = r#"{"name":"Protocol","type":"string","jsonPath":".spec.protocol"}"#,
    printcolumn = r#"{"name":"Active","type":"boolean","jsonPath":".status.active"}"#,
    printcolumn = r#"{"name":"External IP","type":"string","jsonPath":".status.externalIP"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct PortMappingSpec {
    pub external_port: u16,
    pub internal_host: String,
    pub internal_port: u16,
    pub protocol: Protocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_display() {
        assert_eq!(Protocol::Tcp.to_string(), "TCP");
        assert_eq!(Protocol::Udp.to_string(), "UDP");
    }

    #[test]
    fn test_port_mapping_status_default() {
        let status = PortMappingStatus::default();
        assert!(!status.active);
        assert!(status.external_ip.is_none());
        assert!(status.conditions.is_empty());
    }
}
