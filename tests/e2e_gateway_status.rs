//! E2E test: GatewayStatus reconciler against live cluster API.
//!
//! Uses the kube.rs integration test pattern: create a Client, build a Context,
//! apply a CR into the cluster, call reconcile() directly, then assert on status.
//!
//! Requires:
//! - A running cluster (minikube) with CRDs installed
//! - GatewayStatus/default singleton must exist (created by the running controller)
//! - `cargo test --features e2e --test e2e_gateway_status`
//!
//! The running controller will overwrite our test values on its next 60s requeue,
//! so no explicit cleanup is needed.

#[cfg(feature = "e2e")]
mod tests {
    // Kube client and API types
    // use kube::api::{Api, Patch, PatchParams};
    // use kube::Client;
    // use std::sync::Arc;
    // use tokio::sync::RwLock;

    // Our controller and CRD types
    // use upnp_controller::controllers::gateway_ctrl::{self, GatewayContext};
    // use upnp_controller::crds::gateway_status::GatewayStatus;
    // use upnp_controller::metrics::Metrics;
    // use upnp_controller::upnp::eventing::EventingState;
    // use upnp_controller::config::Config;

    /// Test that calling reconcile() directly populates GatewayStatus fields.
    ///
    /// Steps:
    /// 1. Client::try_default() to connect to current kubeconfig
    /// 2. Build a GatewayContext with:
    ///    - Real kube client
    ///    - EventingState pre-loaded with external_ip = Some("10.99.99.99")
    ///      (synthetic IP so we can distinguish from the real controller's value)
    ///    - gateway_url = "http://test-gateway/rootDesc.xml"
    ///    - Metrics::new()
    ///    - config with annotate_nodes: false (avoid clobbering real annotation)
    ///    - node_name: None
    ///    - Empty subscription_id
    /// 3. Fetch current GatewayStatus/default, wrap in Arc
    /// 4. Call gateway_ctrl::reconcile(Arc::new(gs), ctx) directly
    /// 5. Fetch GatewayStatus/default again via API
    /// 6. Assert: externalIP == "10.99.99.99"
    /// 7. Assert: gatewayURL == "http://test-gateway/rootDesc.xml"
    /// 8. Assert: ready == true
    #[tokio::test]
    async fn test_reconcile_sets_status() {
        // TODO: implement
    }
}
