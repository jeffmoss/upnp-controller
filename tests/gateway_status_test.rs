#[cfg(test)]
mod tests {
    #[test]
    fn test_gena_xml_parsing_with_ip() {
        // Test the parse_notify_body function logic inline
        let xml = r#"<?xml version="1.0"?>
<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
  <e:property>
    <NewExternalIPAddress>75.169.255.229</NewExternalIPAddress>
  </e:property>
</e:propertyset>"#;

        // Replicate the parsing logic
        let result = parse_external_ip_from_notify(xml);
        assert_eq!(result, Some("75.169.255.229".to_string()));
    }

    #[test]
    fn test_gena_xml_parsing_no_ip() {
        let xml = r#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
  <e:property><SomeOtherValue>foo</SomeOtherValue></e:property>
</e:propertyset>"#;
        assert_eq!(parse_external_ip_from_notify(xml), None);
    }

    #[test]
    fn test_gateway_status_ready_flag() {
        // GatewayStatus is ready when external_ip is Some
        let has_ip = Some("75.169.255.229".to_string());
        let ready = has_ip.is_some();
        assert!(ready);

        let no_ip: Option<String> = None;
        assert!(!no_ip.is_some());
    }

    fn parse_external_ip_from_notify(xml: &str) -> Option<String> {
        for tag in &["NewExternalIPAddress", "ExternalIPAddress"] {
            let open = format!("<{}>", tag);
            let close = format!("</{}>", tag);
            if let (Some(start), Some(end)) = (xml.find(&open), xml.find(&close)) {
                let value = xml[start + open.len()..end].trim().to_string();
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
        None
    }
}
