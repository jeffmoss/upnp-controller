use std::time::Duration;

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::Client;
use tokio::net::UdpSocket;
use tracing::{debug, info};

const SSDP_MULTICAST_ADDR: &str = "239.255.255.250:1900";

const SSDP_SEARCH: &str = "\
M-SEARCH * HTTP/1.1\r\n\
HOST: 239.255.255.250:1900\r\n\
MAN: \"ssdp:discover\"\r\n\
MX: 2\r\n\
ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\
\r\n";

/// Send SSDP M-SEARCH multicast and wait for an InternetGatewayDevice response.
/// Returns the LOCATION URL from the first response, or None on timeout.
pub async fn ssdp_discover(timeout: Duration) -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    socket.send_to(SSDP_SEARCH.as_bytes(), SSDP_MULTICAST_ADDR).await.ok()?;

    let mut buf = [0u8; 2048];
    match tokio::time::timeout(timeout, socket.recv_from(&mut buf)).await {
        Ok(Ok((len, _addr))) => {
            let response = String::from_utf8_lossy(&buf[..len]);
            parse_ssdp_location(&response)
        }
        _ => None,
    }
}

/// Parse the LOCATION header from an SSDP M-SEARCH response.
pub fn parse_ssdp_location(response: &str) -> Option<String> {
    for line in response.lines() {
        if let Some(value) = line.strip_prefix("LOCATION:").or_else(|| line.strip_prefix("Location:")) {
            return Some(value.trim().to_string());
        }
        // Case-insensitive fallback
        if line.len() > 9 && line[..9].eq_ignore_ascii_case("location:") {
            return Some(line[9..].trim().to_string());
        }
    }
    None
}

#[derive(Debug, Clone)]
pub struct GatewayServices {
    #[allow(dead_code)]
    pub root_url: String,
    pub wan_ip_control_url: Option<String>,
    pub wan_ip_event_url: Option<String>,
    pub wan_ppp_control_url: Option<String>,
    pub wan_ppp_event_url: Option<String>,
}

impl GatewayServices {
    /// Returns the best available control URL (WANIPConnection preferred over WANPPPConnection)
    pub fn control_url(&self) -> Option<&str> {
        self.wan_ip_control_url
            .as_deref()
            .or(self.wan_ppp_control_url.as_deref())
    }

    /// Returns the best available event subscription URL
    pub fn event_url(&self) -> Option<&str> {
        self.wan_ip_event_url
            .as_deref()
            .or(self.wan_ppp_event_url.as_deref())
    }
}

/// Fetch and parse the rootDesc.xml from the gateway, extracting WANIPConnection/WANPPPConnection service URLs
pub async fn discover_gateway(root_desc_url: &str) -> Result<GatewayServices> {
    let client = Client::new();
    let base_url = base_url_from(root_desc_url);

    info!("Fetching rootDesc.xml from {}", root_desc_url);
    let xml = client
        .get(root_desc_url)
        .send()
        .await
        .context("Failed to fetch rootDesc.xml")?
        .text()
        .await
        .context("Failed to read rootDesc.xml body")?;

    debug!("Parsing rootDesc.xml ({} bytes)", xml.len());
    parse_root_desc(&xml, &base_url, root_desc_url)
}

fn base_url_from(url: &str) -> String {
    // Extract http://host:port from URL
    if let Some(idx) = url.find("://") {
        let after_scheme = &url[idx + 3..];
        if let Some(slash_idx) = after_scheme.find('/') {
            let host_port = &after_scheme[..slash_idx];
            let scheme = &url[..idx];
            return format!("{}://{}", scheme, host_port);
        }
    }
    url.to_string()
}

fn parse_root_desc(xml: &str, base_url: &str, root_url: &str) -> Result<GatewayServices> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut services = GatewayServices {
        root_url: root_url.to_string(),
        wan_ip_control_url: None,
        wan_ip_event_url: None,
        wan_ppp_control_url: None,
        wan_ppp_event_url: None,
    };

    let mut current_service_type = String::new();
    let mut current_control_url = String::new();
    let mut current_event_url = String::new();
    let mut in_service = false;
    let mut current_tag = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                current_tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if current_tag == "service" {
                    in_service = true;
                    current_service_type.clear();
                    current_control_url.clear();
                    current_event_url.clear();
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().to_string();
                if in_service {
                    match current_tag.as_str() {
                        "serviceType" => current_service_type = text,
                        "controlURL" => current_control_url = text,
                        "eventSubURL" => current_event_url = text,
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "service" && in_service {
                    let control = if current_control_url.starts_with('/') {
                        format!("{}{}", base_url, current_control_url)
                    } else {
                        current_control_url.clone()
                    };
                    let event = if current_event_url.starts_with('/') {
                        format!("{}{}", base_url, current_event_url)
                    } else {
                        current_event_url.clone()
                    };

                    if current_service_type.contains("WANIPConnection") {
                        debug!("Found WANIPConnection: control={}, event={}", control, event);
                        services.wan_ip_control_url = Some(control);
                        services.wan_ip_event_url = Some(event);
                    } else if current_service_type.contains("WANPPPConnection") {
                        debug!("Found WANPPPConnection: control={}, event={}", control, event);
                        services.wan_ppp_control_url = Some(control);
                        services.wan_ppp_event_url = Some(event);
                    }
                    in_service = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("XML parse error: {}", e)),
            _ => {}
        }
    }

    Ok(services)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_ROOT_DESC: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceList>
      <device>
        <deviceList>
          <device>
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

    #[test]
    fn test_parse_root_desc() {
        let services = parse_root_desc(SAMPLE_ROOT_DESC, "http://192.168.0.1:5000", "http://192.168.0.1:5000/rootDesc.xml").unwrap();
        assert!(services.wan_ip_control_url.is_some());
        assert_eq!(
            services.wan_ip_control_url.unwrap(),
            "http://192.168.0.1:5000/upnp/control/WANIPConn1"
        );
    }

    #[test]
    fn test_base_url_from() {
        assert_eq!(
            base_url_from("http://192.168.0.1:5000/rootDesc.xml"),
            "http://192.168.0.1:5000"
        );
    }

    #[test]
    fn test_parse_ssdp_location() {
        let response = "HTTP/1.1 200 OK\r\n\
            CACHE-CONTROL: max-age=1800\r\n\
            ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\
            LOCATION: http://192.168.0.1:5000/rootDesc.xml\r\n\
            SERVER: Linux UPnP/1.0\r\n\
            \r\n";
        assert_eq!(
            parse_ssdp_location(response),
            Some("http://192.168.0.1:5000/rootDesc.xml".to_string())
        );
    }

    #[test]
    fn test_parse_ssdp_location_missing() {
        let response = "HTTP/1.1 200 OK\r\nST: something\r\n\r\n";
        assert_eq!(parse_ssdp_location(response), None);
    }

    #[test]
    fn test_parse_ssdp_location_case_insensitive() {
        let response = "HTTP/1.1 200 OK\r\nlocation: http://10.0.0.1:80/desc.xml\r\n\r\n";
        assert_eq!(
            parse_ssdp_location(response),
            Some("http://10.0.0.1:80/desc.xml".to_string())
        );
    }
}
