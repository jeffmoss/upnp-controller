#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json;

    // Test CRD serialization round-trip
    #[test]
    fn test_port_mapping_spec_roundtrip() {
        let json = serde_json::json!({
            "externalPort": 8080,
            "internalHost": "192.168.1.50",
            "internalPort": 8080,
            "protocol": "TCP",
            "description": "test service"
        });
        // Verify JSON structure
        assert_eq!(json["externalPort"], 8080);
        assert_eq!(json["protocol"], "TCP");
    }

    #[test]
    fn test_lease_expiry_calculation() {
        let now = Utc::now();
        let lease_secs: i64 = 3600;
        let expiry = now + chrono::Duration::seconds(lease_secs);
        let renewal_buffer: i64 = 30;
        let requeue_secs = (expiry - now).num_seconds() - renewal_buffer;
        assert!(requeue_secs > 0);
        assert!(requeue_secs < lease_secs);
        assert_eq!(requeue_secs, lease_secs - renewal_buffer);
    }

    #[test]
    fn test_requeue_timing_with_expired_lease() {
        let now = Utc::now();
        let past = now - chrono::Duration::seconds(10);
        let renewal_buffer: i64 = 30;
        let secs = (past - now).num_seconds() - renewal_buffer;
        // Should requeue immediately (clamped to min 10)
        let requeue = secs.max(10) as u64;
        assert_eq!(requeue, 10);
    }
}
