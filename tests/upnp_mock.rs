/// Mock UPnP server for integration testing.
///
/// This module provides a mock UPnP IGD server built with axum that responds
/// to SUBSCRIBE, AddPortMapping, DeletePortMapping, and GetExternalIPAddress
/// SOAP actions.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use tokio::net::TcpListener;

#[derive(Debug, Clone, Default)]
pub struct MockState {
    pub port_mappings: Arc<Mutex<HashMap<(u16, String), MockPortMapping>>>,
    pub external_ip: Arc<Mutex<String>>,
    pub subscriptions: Arc<Mutex<Vec<String>>>,
}

#[derive(Debug, Clone)]
pub struct MockPortMapping {
    pub external_port: u16,
    pub protocol: String,
    pub internal_host: String,
    pub internal_port: u16,
    pub description: String,
}

impl MockState {
    pub fn new(external_ip: &str) -> Self {
        Self {
            port_mappings: Arc::new(Mutex::new(HashMap::new())),
            external_ip: Arc::new(Mutex::new(external_ip.to_string())),
            subscriptions: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

async fn handle_root_desc() -> impl IntoResponse {
    let xml = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>
    <deviceList>
      <device>
        <deviceType>urn:schemas-upnp-org:device:WANDevice:1</deviceType>
        <deviceList>
          <device>
            <deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType>
            <serviceList>
              <service>
                <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
                <serviceId>urn:upnp-org:serviceId:WANIPConn1</serviceId>
                <controlURL>/upnp/control/WANIPConn1</controlURL>
                <eventSubURL>/upnp/event/WANIPConn1</eventSubURL>
              </service>
            </serviceList>
          </device>
        </deviceList>
      </device>
    </deviceList>
  </device>
</root>"#;
    (StatusCode::OK, [("Content-Type", "text/xml")], xml)
}

async fn handle_subscribe(
    State(state): State<MockState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let _callback = headers
        .get("CALLBACK")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<http://unknown>")
        .to_string();

    let sid = format!("uuid:mock-sid-{}", uuid::Uuid::new_v4());
    state.subscriptions.lock().unwrap().push(sid.clone());

    (
        StatusCode::OK,
        [
            ("SID", sid),
            ("TIMEOUT", "Second-1800".to_string()),
        ],
    )
}

async fn handle_control(
    State(state): State<MockState>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let soap_action = headers
        .get("SOAPAction")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if soap_action.contains("AddPortMapping") {
        let external_port = extract_value(&body, "NewExternalPort")
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(0);
        let protocol = extract_value(&body, "NewProtocol").unwrap_or_default();
        let internal_host = extract_value(&body, "NewInternalClient").unwrap_or_default();
        let internal_port = extract_value(&body, "NewInternalPort")
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(0);
        let description = extract_value(&body, "NewPortMappingDescription").unwrap_or_default();

        state.port_mappings.lock().unwrap().insert(
            (external_port, protocol.clone()),
            MockPortMapping {
                external_port,
                protocol,
                internal_host,
                internal_port,
                description,
            },
        );

        let response = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:AddPortMappingResponse xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1"/>
  </s:Body>
</s:Envelope>"#;
        (StatusCode::OK, [("Content-Type", "text/xml")], response.to_string())

    } else if soap_action.contains("DeletePortMapping") {
        let external_port = extract_value(&body, "NewExternalPort")
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(0);
        let protocol = extract_value(&body, "NewProtocol").unwrap_or_default();
        state.port_mappings.lock().unwrap().remove(&(external_port, protocol));

        let response = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:DeletePortMappingResponse xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1"/>
  </s:Body>
</s:Envelope>"#;
        (StatusCode::OK, [("Content-Type", "text/xml")], response.to_string())

    } else if soap_action.contains("GetExternalIPAddress") {
        let ip = state.external_ip.lock().unwrap().clone();
        let response = format!(r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:GetExternalIPAddressResponse xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1">
      <NewExternalIPAddress>{}</NewExternalIPAddress>
    </u:GetExternalIPAddressResponse>
  </s:Body>
</s:Envelope>"#, ip);
        (StatusCode::OK, [("Content-Type", "text/xml")], response)

    } else if soap_action.contains("GetGenericPortMappingEntry") {
        // Return error 713 (SpecifiedArrayIndexInvalid) for simplicity
        let response = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <s:Fault>
      <faultcode>s:Client</faultcode>
      <faultstring>UPnPError</faultstring>
      <detail>
        <UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
          <errorCode>713</errorCode>
          <errorDescription>SpecifiedArrayIndexInvalid</errorDescription>
        </UPnPError>
      </detail>
    </s:Fault>
  </s:Body>
</s:Envelope>"#;
        (StatusCode::INTERNAL_SERVER_ERROR, [("Content-Type", "text/xml")], response.to_string())

    } else {
        (StatusCode::BAD_REQUEST, [("Content-Type", "text/xml")], "Unknown action".to_string())
    }
}

fn extract_value(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)? + open.len();
    let end = xml.find(&close)?;
    Some(xml[start..end].trim().to_string())
}

pub async fn start_mock_server(state: MockState) -> SocketAddr {
    let app = Router::new()
        .route("/rootDesc.xml", get(handle_root_desc))
        .route("/upnp/event/WANIPConn1", axum::routing::any(handle_subscribe))
        .route("/upnp/control/WANIPConn1", post(handle_control))
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Client;

    #[tokio::test]
    async fn test_mock_get_external_ip() {
        let state = MockState::new("75.169.255.229");
        let addr = start_mock_server(state).await;

        let client = Client::new();
        let soap_body = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:GetExternalIPAddress xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1"/>
  </s:Body>
</s:Envelope>"#;

        let resp = client
            .post(format!("http://{}/upnp/control/WANIPConn1", addr))
            .header("SOAPAction", r#""urn:schemas-upnp-org:service:WANIPConnection:1#GetExternalIPAddress""#)
            .header("Content-Type", "text/xml")
            .body(soap_body)
            .send()
            .await
            .unwrap();

        assert!(resp.status().is_success());
        let body = resp.text().await.unwrap();
        assert!(body.contains("75.169.255.229"));
    }

    #[tokio::test]
    async fn test_mock_add_and_delete_port_mapping() {
        let state = MockState::new("75.169.255.229");
        let addr = start_mock_server(state.clone()).await;
        let client = Client::new();

        // Add mapping
        let add_body = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:AddPortMapping xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1">
      <NewRemoteHost></NewRemoteHost>
      <NewExternalPort>8080</NewExternalPort>
      <NewProtocol>TCP</NewProtocol>
      <NewInternalPort>8080</NewInternalPort>
      <NewInternalClient>192.168.1.50</NewInternalClient>
      <NewEnabled>1</NewEnabled>
      <NewPortMappingDescription>test</NewPortMappingDescription>
      <NewLeaseDuration>3600</NewLeaseDuration>
    </u:AddPortMapping>
  </s:Body>
</s:Envelope>"#;

        let resp = client
            .post(format!("http://{}/upnp/control/WANIPConn1", addr))
            .header("SOAPAction", r#""urn:schemas-upnp-org:service:WANIPConnection:1#AddPortMapping""#)
            .header("Content-Type", "text/xml")
            .body(add_body)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        // Verify it was stored
        assert!(state.port_mappings.lock().unwrap().contains_key(&(8080, "TCP".to_string())));

        // Delete mapping
        let delete_body = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:DeletePortMapping xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1">
      <NewRemoteHost></NewRemoteHost>
      <NewExternalPort>8080</NewExternalPort>
      <NewProtocol>TCP</NewProtocol>
    </u:DeletePortMapping>
  </s:Body>
</s:Envelope>"#;

        let resp = client
            .post(format!("http://{}/upnp/control/WANIPConn1", addr))
            .header("SOAPAction", r#""urn:schemas-upnp-org:service:WANIPConnection:1#DeletePortMapping""#)
            .header("Content-Type", "text/xml")
            .body(delete_body)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        // Verify it was removed
        assert!(!state.port_mappings.lock().unwrap().contains_key(&(8080, "TCP".to_string())));
    }

    #[tokio::test]
    async fn test_mock_subscribe() {
        let state = MockState::new("75.169.255.229");
        let addr = start_mock_server(state.clone()).await;
        let client = Client::new();

        let resp = client
            .request(
                reqwest::Method::from_bytes(b"SUBSCRIBE").unwrap(),
                format!("http://{}/upnp/event/WANIPConn1", addr),
            )
            .header("NT", "upnp:event")
            .header("CALLBACK", "<http://127.0.0.1:9091/notify>")
            .header("TIMEOUT", "Second-1800")
            .send()
            .await
            .unwrap();

        assert!(resp.status().is_success());
        assert!(resp.headers().contains_key("SID") || resp.headers().contains_key("sid"));
    }
}
