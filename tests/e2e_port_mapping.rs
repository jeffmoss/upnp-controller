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
//! - A running cluster (minikube) with CRDs installed
//! - `cargo test --features e2e --test e2e_port_mapping`

#[cfg(feature = "e2e")]
mod tests {
    // Import the mock UPnP server from the sibling test file
    // #[path = "upnp_mock.rs"]
    // mod upnp_mock;

    // Kube client and API types
    // use kube::api::{Api, Patch, PatchParams, DeleteParams};
    // use kube::Client;
    // use std::sync::Arc;

    // Our controller, CRD, and UPnP types
    // use upnp_controller::controllers::port_mapping_ctrl::{self, PortMappingContext};
    // use upnp_controller::crds::port_mapping::{PortMapping, PortMappingSpec, Protocol};
    // use upnp_controller::upnp::port_mapping::UpnpClient;
    // use upnp_controller::metrics::Metrics;

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
        // TODO: implement
    }
}
