use anyhow::{Context, Result};
use reqwest::Client;
use tracing::{debug, info};

const SOAP_NAMESPACE: &str = "urn:schemas-upnp-org:service:WANIPConnection:1";

pub struct UpnpClient {
    http: Client,
    control_url: String,
}

impl UpnpClient {
    pub fn new(control_url: String) -> Self {
        Self {
            http: Client::new(),
            control_url,
        }
    }

    async fn soap_action(&self, action: &str, body: &str) -> Result<String> {
        let soap_body = format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"
            s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>{}</s:Body>
</s:Envelope>"#,
            body
        );

        let soap_action = format!("\"{}#{}\"", SOAP_NAMESPACE, action);
        debug!("SOAP {} -> {}", action, self.control_url);

        let response = self
            .http
            .post(&self.control_url)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header("SOAPAction", soap_action)
            .body(soap_body)
            .send()
            .await
            .context("SOAP request failed")?;

        let status = response.status();
        let text = response.text().await.context("Failed to read SOAP response")?;
        if !status.is_success() {
            return Err(anyhow::anyhow!("SOAP error {}: {}", status, text));
        }
        Ok(text)
    }

    /// Add or renew a port mapping. Returns the lease duration granted (0 = permanent).
    pub async fn add_port_mapping(
        &self,
        external_port: u16,
        internal_host: &str,
        internal_port: u16,
        protocol: &str,
        description: &str,
        lease_duration: u32,
    ) -> Result<u32> {
        let body = format!(
            r#"<u:AddPortMapping xmlns:u="{}">
  <NewRemoteHost></NewRemoteHost>
  <NewExternalPort>{}</NewExternalPort>
  <NewProtocol>{}</NewProtocol>
  <NewInternalPort>{}</NewInternalPort>
  <NewInternalClient>{}</NewInternalClient>
  <NewEnabled>1</NewEnabled>
  <NewPortMappingDescription>{}</NewPortMappingDescription>
  <NewLeaseDuration>{}</NewLeaseDuration>
</u:AddPortMapping>"#,
            SOAP_NAMESPACE,
            external_port,
            protocol,
            internal_port,
            internal_host,
            description,
            lease_duration,
        );

        self.soap_action("AddPortMapping", &body).await?;
        info!(
            "Added port mapping: {}:{} -> {}:{} ({})",
            external_port, protocol, internal_host, internal_port, description
        );
        Ok(lease_duration)
    }

    /// Delete a port mapping
    pub async fn delete_port_mapping(
        &self,
        external_port: u16,
        protocol: &str,
    ) -> Result<()> {
        let body = format!(
            r#"<u:DeletePortMapping xmlns:u="{}">
  <NewRemoteHost></NewRemoteHost>
  <NewExternalPort>{}</NewExternalPort>
  <NewProtocol>{}</NewProtocol>
</u:DeletePortMapping>"#,
            SOAP_NAMESPACE, external_port, protocol
        );

        self.soap_action("DeletePortMapping", &body).await?;
        info!("Deleted port mapping: {} {}", external_port, protocol);
        Ok(())
    }

    /// Get the current external (WAN) IP address
    pub async fn get_external_ip(&self) -> Result<String> {
        let body = format!(
            r#"<u:GetExternalIPAddress xmlns:u="{}"></u:GetExternalIPAddress>"#,
            SOAP_NAMESPACE
        );

        let response = self.soap_action("GetExternalIPAddress", &body).await?;
        parse_external_ip(&response)
    }

    /// Get a specific port mapping by index
    #[allow(dead_code)]
    pub async fn get_generic_port_mapping_entry(&self, index: u32) -> Result<Option<PortMappingEntry>> {
        let body = format!(
            r#"<u:GetGenericPortMappingEntry xmlns:u="{}">
  <NewPortMappingIndex>{}</NewPortMappingIndex>
</u:GetGenericPortMappingEntry>"#,
            SOAP_NAMESPACE, index
        );

        match self.soap_action("GetGenericPortMappingEntry", &body).await {
            Ok(response) => Ok(Some(parse_port_mapping_entry(&response)?)),
            Err(e) if e.to_string().contains("SpecifiedArrayIndexInvalid") || e.to_string().contains("713") => {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// List all current port mappings
    #[allow(dead_code)]
    pub async fn list_port_mappings(&self) -> Result<Vec<PortMappingEntry>> {
        let mut mappings = Vec::new();
        let mut index = 0u32;
        while let Some(entry) = self.get_generic_port_mapping_entry(index).await? {
            mappings.push(entry);
            index += 1;
        }
        Ok(mappings)
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PortMappingEntry {
    pub external_port: u16,
    pub protocol: String,
    pub internal_client: String,
    pub internal_port: u16,
    pub description: String,
    pub enabled: bool,
    pub lease_duration: u32,
}

fn parse_external_ip(xml: &str) -> Result<String> {
    extract_xml_value(xml, "NewExternalIPAddress")
        .context("ExternalIPAddress not found in response")
}

#[allow(dead_code)]
fn parse_port_mapping_entry(xml: &str) -> Result<PortMappingEntry> {
    Ok(PortMappingEntry {
        external_port: extract_xml_value(xml, "NewExternalPort")?
            .parse()
            .context("Invalid external port")?,
        protocol: extract_xml_value(xml, "NewProtocol")?,
        internal_client: extract_xml_value(xml, "NewInternalClient")?,
        internal_port: extract_xml_value(xml, "NewInternalPort")?
            .parse()
            .context("Invalid internal port")?,
        description: extract_xml_value(xml, "NewPortMappingDescription").unwrap_or_default(),
        enabled: extract_xml_value(xml, "NewEnabled").map(|v| v == "1").unwrap_or(false),
        lease_duration: extract_xml_value(xml, "NewLeaseDuration")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    })
}

fn extract_xml_value(xml: &str, tag: &str) -> Result<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    if let (Some(start), Some(end)) = (xml.find(&open), xml.find(&close)) {
        let value = &xml[start + open.len()..end];
        Ok(value.to_string())
    } else {
        Err(anyhow::anyhow!("Tag {} not found in XML", tag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_external_ip() {
        let xml = r#"<s:Envelope><s:Body><u:GetExternalIPAddressResponse><NewExternalIPAddress>75.169.255.229</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>"#;
        assert_eq!(parse_external_ip(xml).unwrap(), "75.169.255.229");
    }

    #[test]
    fn test_extract_xml_value() {
        let xml = "<root><Foo>bar</Foo></root>";
        assert_eq!(extract_xml_value(xml, "Foo").unwrap(), "bar");
        assert!(extract_xml_value(xml, "Missing").is_err());
    }
}
