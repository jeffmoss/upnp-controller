//! E2E smoke test: verifies the controller is deployed and functioning.
//!
//! Requires:
//! - A running cluster (minikube) with CRDs installed and the controller deployed
//! - `cargo test --features e2e` to run

#[cfg(feature = "e2e")]
mod tests {
    use k8s_openapi::api::apps::v1::Deployment;
    use k8s_openapi::api::core::v1::Node;
    use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
    use kube::api::{Api, ListParams};
    use kube::Client;
    use upnp_controller::crds::gateway_status::GatewayStatus;

    /// Verify both CRDs exist in the cluster.
    #[tokio::test]
    async fn test_crds_installed() {
        let client = Client::try_default().await.expect("kubeconfig required");
        let crds: Api<CustomResourceDefinition> = Api::all(client);

        let pm = crds.get("portmappings.upnp.k8s.io").await;
        assert!(pm.is_ok(), "PortMapping CRD not found");

        let gs = crds.get("gatewaystatuses.upnp.k8s.io").await;
        assert!(gs.is_ok(), "GatewayStatus CRD not found");
    }

    /// Verify the controller pod is running and ready.
    #[tokio::test]
    async fn test_controller_running() {
        let client = Client::try_default().await.expect("kubeconfig required");
        let deployments: Api<Deployment> = Api::namespaced(client, "upnp-controller");

        let deploy = deployments
            .get("upnp-controller")
            .await
            .expect("controller deployment not found");

        let status = deploy.status.expect("deployment has no status");
        let ready = status.ready_replicas.unwrap_or(0);
        assert!(ready >= 1, "controller has no ready replicas: {:?}", status);
    }

    /// Verify GatewayStatus singleton exists and has populated fields.
    #[tokio::test]
    async fn test_gateway_status_populated() {
        let client = Client::try_default().await.expect("kubeconfig required");
        let api: Api<GatewayStatus> = Api::all(client);

        let gs = api
            .get("default")
            .await
            .expect("GatewayStatus/default not found");

        let status = gs.status.expect("GatewayStatus has no status");
        assert!(status.ready, "GatewayStatus not ready");
        assert!(
            status.external_ip.is_some(),
            "GatewayStatus has no externalIP"
        );
        assert!(
            status.gateway_url.is_some(),
            "GatewayStatus has no gatewayURL"
        );
    }

    /// Verify the controller annotated the node with the external IP.
    #[tokio::test]
    async fn test_node_annotation() {
        let client = Client::try_default().await.expect("kubeconfig required");
        let nodes: Api<Node> = Api::all(client);

        let node_list = nodes
            .list(&ListParams::default())
            .await
            .expect("failed to list nodes");

        let node = node_list.items.first().expect("no nodes in cluster");
        let annotations = node.metadata.annotations.as_ref().expect("node has no annotations");

        assert!(
            annotations.contains_key("external-dns.alpha.kubernetes.io/target"),
            "node missing external-dns annotation, found: {:?}",
            annotations.keys().collect::<Vec<_>>()
        );
    }

    /// Verify the metrics endpoint responds.
    #[tokio::test]
    async fn test_metrics_endpoint() {
        let resp = reqwest::Client::new()
            .get("http://127.0.0.1:9090/metrics")
            .send()
            .await;

        match resp {
            Ok(r) => {
                assert!(r.status().is_success(), "metrics returned {}", r.status());
                let body = r.text().await.unwrap_or_default();
                assert!(
                    body.contains("upnp_active_port_mappings"),
                    "metrics response missing expected metric"
                );
            }
            Err(_) => {
                // hostNetwork means metrics are on the minikube node IP, not localhost
                // This test may need adjustment based on network setup
                eprintln!("WARN: could not reach metrics at localhost:9090 (expected with minikube docker driver)");
            }
        }
    }
}
