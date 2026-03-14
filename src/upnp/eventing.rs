use anyhow::{Context, Result};
use reqwest::Client;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{Duration, Instant};
use tracing::{debug, info, warn};

pub struct GenaSubscription {
    pub sid: String,
    pub expiry: Instant,
}

pub struct EventingState {
    pub subscription: RwLock<Option<GenaSubscription>>,
    pub last_notify: RwLock<Option<Instant>>,
    pub current_external_ip: RwLock<Option<String>>,
}

impl EventingState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            subscription: RwLock::new(None),
            last_notify: RwLock::new(None),
            current_external_ip: RwLock::new(None),
        })
    }
}

/// Subscribe to GENA events for ExternalIPAddress changes.
/// Returns the subscription SID.
pub async fn subscribe(
    event_url: &str,
    callback_url: &str,
    timeout_secs: u32,
) -> Result<String> {
    let client = Client::new();
    let response = client
        .request(reqwest::Method::from_bytes(b"SUBSCRIBE").unwrap(), event_url)
        .header("NT", "upnp:event")
        .header("CALLBACK", format!("<{}>", callback_url))
        .header("TIMEOUT", format!("Second-{}", timeout_secs))
        .send()
        .await
        .context("SUBSCRIBE request failed")?;

    let status = response.status();
    if !status.is_success() {
        return Err(anyhow::anyhow!("SUBSCRIBE failed with status {}", status));
    }

    let sid = response
        .headers()
        .get("SID")
        .context("No SID in SUBSCRIBE response")?
        .to_str()
        .context("Invalid SID header")?
        .to_string();

    info!("GENA subscribed: SID={}, TTL={}s", sid, timeout_secs);
    Ok(sid)
}

/// Renew an existing GENA subscription
pub async fn renew_subscription(
    event_url: &str,
    sid: &str,
    timeout_secs: u32,
) -> Result<()> {
    let client = Client::new();
    let response = client
        .request(reqwest::Method::from_bytes(b"SUBSCRIBE").unwrap(), event_url)
        .header("SID", sid)
        .header("TIMEOUT", format!("Second-{}", timeout_secs))
        .send()
        .await
        .context("SUBSCRIBE renewal failed")?;

    let status = response.status();
    if !status.is_success() {
        return Err(anyhow::anyhow!("SUBSCRIBE renewal failed: {}", status));
    }
    debug!("GENA subscription renewed: SID={}", sid);
    Ok(())
}

/// Unsubscribe from GENA events
#[allow(dead_code)]
pub async fn unsubscribe(event_url: &str, sid: &str) -> Result<()> {
    let client = Client::new();
    client
        .request(reqwest::Method::from_bytes(b"UNSUBSCRIBE").unwrap(), event_url)
        .header("SID", sid)
        .send()
        .await
        .context("UNSUBSCRIBE failed")?;
    info!("GENA unsubscribed: SID={}", sid);
    Ok(())
}

/// Parse a GENA NOTIFY body and extract the ExternalIPAddress value
pub fn parse_notify_body(xml: &str) -> Option<String> {
    // Look for <NewExternalIPAddress> or <ExternalIPAddress> in the property set
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

/// Start the GENA renewal loop. Renews at half the TTL interval.
pub async fn run_renewal_loop(
    event_url: String,
    state: Arc<EventingState>,
    ttl_secs: u32,
) {
    let renewal_interval = Duration::from_secs((ttl_secs / 2) as u64);
    loop {
        tokio::time::sleep(renewal_interval).await;
        let sid = {
            let sub = state.subscription.read().await;
            sub.as_ref().map(|s| s.sid.clone())
        };
        if let Some(sid) = sid {
            match renew_subscription(&event_url, &sid, ttl_secs).await {
                Ok(()) => {
                    let mut sub = state.subscription.write().await;
                    if let Some(s) = sub.as_mut() {
                        s.expiry = Instant::now() + Duration::from_secs(ttl_secs as u64);
                    }
                }
                Err(e) => {
                    warn!("GENA renewal failed: {}; will resubscribe", e);
                    // Clear subscription so main loop will resubscribe
                    *state.subscription.write().await = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_notify_body() {
        let xml = r#"<?xml version="1.0"?>
<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
  <e:property>
    <NewExternalIPAddress>75.169.255.229</NewExternalIPAddress>
  </e:property>
</e:propertyset>"#;
        assert_eq!(
            parse_notify_body(xml),
            Some("75.169.255.229".to_string())
        );
    }

    #[test]
    fn test_parse_notify_body_empty() {
        let xml = "<e:propertyset><e:property><SomeOtherProp>value</SomeOtherProp></e:property></e:propertyset>";
        assert_eq!(parse_notify_body(xml), None);
    }
}
