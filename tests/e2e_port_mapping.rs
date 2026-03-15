//! E2E test: PortMapping reconciler lifecycle against live cluster API + mock UPnP.
//!
//! Uses the kube.rs integration test pattern: create a Client, build a Context,
//! apply a CR into the cluster, call reconcile() directly, then assert on status.
//!
//! The mock UPnP server (from tests/upnp_mock.rs) stands in for the real router,
//! so we can verify port mappings are created and cleaned up without touching
//! actual hardware.
//!
//! Requires:
//! - A running cluster (minikube with kvm2 driver) with CRDs installed
//! - `cargo test --features e2e_kvm --test e2e_port_mapping`

#[cfg(feature = "e2e_kvm")]
#[path = "upnp_mock.rs"]
mod upnp_mock;

#[cfg(feature = "e2e_kvm")]
mod tests {
    use kube::api::{Api, DeleteParams, Patch, PatchParams};
    use kube::Client;
    use std::sync::Arc;

    use upnp_controller::controllers::port_mapping_ctrl::{self, PortMappingContext};
    use upnp_controller::crds::port_mapping::PortMapping;
    use upnp_controller::metrics::Metrics;
    use upnp_controller::upnp::port_mapping::UpnpClient;

    use super::upnp_mock;

    /// Verify SSDP multicast discovery finds the gateway from inside the KVM cluster.
    #[tokio::test]
    async fn test_ssdp_discovers_gateway() {
        use upnp_controller::upnp::discovery::ssdp_discover;
        use std::time::Duration;

        let location = ssdp_discover(Duration::from_secs(5)).await;

        assert!(
            location.is_some(),
            "SSDP discovery should find a gateway on the LAN"
        );

        let url = location.unwrap();
        assert!(
            url.starts_with("http://"),
            "SSDP LOCATION should be an HTTP URL, got: {}",
            url
        );

        eprintln!("SSDP discovered gateway: {}", url);
    }

    /// Verify LAN IP detection works from inside the KVM cluster.
    #[tokio::test]
    async fn test_lan_ip_detection() {
        use upnp_controller::config::{detect_lan_ip, parse_host};
        use upnp_controller::upnp::discovery::ssdp_discover;
        use std::time::Duration;

        let location = ssdp_discover(Duration::from_secs(5))
            .await
            .expect("SSDP discovery must find a gateway for this test");

        let host = parse_host(&location)
            .expect("should parse host from SSDP LOCATION");

        let lan_ip = detect_lan_ip(&host)
            .expect("should detect LAN IP via UDP socket trick");

        assert!(!lan_ip.is_empty(), "LAN IP should not be empty");
        assert_ne!(lan_ip, "0.0.0.0", "LAN IP should not be 0.0.0.0");
        assert_ne!(lan_ip, "127.0.0.1", "LAN IP should not be loopback");

        eprintln!("Detected LAN IP: {} (gateway host: {})", lan_ip, host);
    }

    /// Test the full PortMapping lifecycle: create -> active -> delete -> cleanup.
    ///
    /// Steps:
    /// 1. Client::try_default() to connect to current kubeconfig
    /// 2. Start mock UPnP server via upnp_mock::start_mock_server()
    /// 3. Create UpnpClient pointed at mock's control URL
    ///    (format: "http://{addr}/upnp/control/WANIPConn1")
    /// 4. Build PortMappingContext with real client, mock-backed UpnpClient, fresh Metrics
    /// 5. Server-side apply a test PortMapping CR:
    ///    - name: "test-e2e-pm"
    ///    - namespace: "default"
    ///    - externalPort: 19999
    ///    - internalHost: "192.168.0.1"
    ///    - internalPort: 19999
    ///    - protocol: TCP
    ///    - description: "e2e test mapping"
    /// 6. Fetch the applied CR, wrap in Arc
    /// 7. Call port_mapping_ctrl::reconcile(Arc::new(pm), ctx)
    ///    - First call adds the finalizer (upnp.k8s.io/cleanup) and runs reconcile_apply
    ///    - reconcile_apply calls UpnpClient::add_port_mapping on the mock server
    ///    - Then patches status with active=true, externalIP, conditions
    /// 8. Fetch status via API:
    ///    - Assert active == true
    ///    - Assert externalIP is set (mock returns "75.169.255.229")
    ///    - Assert conditions contain reason "MappingEstablished"
    /// 9. Assert mock state: port_mappings contains key (19999, "TCP")
    ///
    /// Delete phase:
    /// 10. Call api.delete("test-e2e-pm") — sets deletionTimestamp, but finalizer
    ///     prevents actual deletion
    /// 11. Fetch the updated CR (now has deletionTimestamp), wrap in Arc
    /// 12. Call reconcile() again — finalizer detects deletion, runs reconcile_cleanup
    ///     which calls UpnpClient::delete_port_mapping on the mock server,
    ///     then removes the finalizer so K8s garbage-collects the CR
    /// 13. Assert mock state: port_mappings no longer contains (19999, "TCP")
    /// 14. Cleanup: verify CR is gone (api.get should return NotFound)
    #[tokio::test]
    async fn test_reconcile_port_mapping_lifecycle() {
        // 1. Connect to cluster
        let client = Client::try_default()
            .await
            .expect("kubeconfig must be available");
        let api: Api<PortMapping> = Api::namespaced(client.clone(), "default");

        // 2. Start mock UPnP server
        let mock_state = upnp_mock::MockState::new("75.169.255.229");
        let addr = upnp_mock::start_mock_server(mock_state.clone()).await;

        // 3. Create UpnpClient pointed at mock
        let control_url = format!("http://{}/upnp/control/WANIPConn1", addr);
        let upnp = Arc::new(UpnpClient::new(control_url));

        // 4. Build context
        let metrics = Metrics::new().expect("metrics init");
        let ctx = Arc::new(PortMappingContext {
            client: client.clone(),
            upnp,
            metrics,
        });

        // 5. Server-side apply a test PortMapping CR
        let pm_manifest = serde_json::json!({
            "apiVersion": "upnp.k8s.io/v1alpha1",
            "kind": "PortMapping",
            "metadata": {
                "name": "test-e2e-pm",
                "namespace": "default"
            },
            "spec": {
                "externalPort": 19999,
                "internalHost": "192.168.0.1",
                "internalPort": 19999,
                "protocol": "TCP",
                "description": "e2e test mapping"
            }
        });
        api.patch(
            "test-e2e-pm",
            &PatchParams::apply("e2e-test"),
            &Patch::Apply(&pm_manifest),
        )
        .await
        .expect("failed to apply PortMapping CR");

        // 6. Fetch the applied CR
        let pm = api.get("test-e2e-pm").await.expect("failed to get CR");

        // 7. First reconcile: adds finalizer + creates mapping
        port_mapping_ctrl::reconcile(Arc::new(pm), ctx.clone())
            .await
            .expect("reconcile failed");

        // The finalizer pass may not run reconcile_apply on the first call
        // (it adds the finalizer, then re-triggers). Reconcile again to ensure
        // the apply path runs.
        let pm = api.get("test-e2e-pm").await.expect("failed to get CR after finalizer");
        port_mapping_ctrl::reconcile(Arc::new(pm), ctx.clone())
            .await
            .expect("second reconcile failed");

        // 8. Fetch status and assert
        let pm = api.get("test-e2e-pm").await.expect("failed to get CR for status check");
        let status = pm.status.as_ref().expect("status should be set");
        assert!(status.active, "status.active should be true");
        assert_eq!(
            status.external_ip.as_deref(),
            Some("75.169.255.229"),
            "externalIP should match mock"
        );
        assert!(
            status.conditions.iter().any(|c| c.reason == "MappingEstablished"),
            "conditions should contain MappingEstablished"
        );

        // 9. Assert mock state has the mapping
        assert!(
            mock_state
                .port_mappings
                .lock()
                .unwrap()
                .contains_key(&(19999, "TCP".to_string())),
            "mock should have the port mapping"
        );

        // 10. Delete the CR (finalizer prevents immediate removal)
        api.delete("test-e2e-pm", &DeleteParams::default())
            .await
            .expect("failed to delete CR");

        // 11. Fetch the CR with deletionTimestamp set
        let pm = api
            .get("test-e2e-pm")
            .await
            .expect("CR should still exist due to finalizer");

        // 12. Reconcile cleanup: removes mapping from mock, removes finalizer
        port_mapping_ctrl::reconcile(Arc::new(pm), ctx.clone())
            .await
            .expect("cleanup reconcile failed");

        // 13. Assert mock state: mapping removed
        assert!(
            !mock_state
                .port_mappings
                .lock()
                .unwrap()
                .contains_key(&(19999, "TCP".to_string())),
            "mock should no longer have the port mapping"
        );

        // 14. Verify CR is gone (k8s garbage-collected after finalizer removal)
        match api.get("test-e2e-pm").await {
            Err(kube::Error::Api(err)) if err.code == 404 => {} // expected
            Err(e) => panic!("unexpected error: {}", e),
            Ok(_) => panic!("CR should have been deleted"),
        }
    }
}
