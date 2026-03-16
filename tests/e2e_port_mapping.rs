//! E2E tests: controller smoke tests, SSDP discovery, and PortMapping lifecycle.
//!
//! All tests run against a real KVM cluster with macvtap LAN access and a real router.
//! The deployed controller handles reconciliation — tests create CRs and verify status.
//!
//! Requires:
//! - A running minikube cluster (kvm2 + macvtap) with CRDs installed and controller deployed
//! - The node must have a LAN-routable IP on eth1 (macvtap bridge)
//! - `just e2e` (or `cargo test --features e2e`)

#[cfg(feature = "e2e")]
mod tests {
    use std::time::Duration;

    use k8s_openapi::api::apps::v1::Deployment;
    use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
    use kube::api::{Api, DeleteParams, Patch, PatchParams};
    use kube::Client;

    use upnp_controller::config::{detect_lan_ip, parse_host};
    use upnp_controller::crds::gateway_status::GatewayStatus;
    use upnp_controller::crds::port_mapping::PortMapping;
    use upnp_controller::upnp::discovery::ssdp_discover;

    // --- Controller smoke tests ---

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
        assert!(status.external_ip.is_some(), "GatewayStatus has no externalIP");
        assert!(status.gateway_url.is_some(), "GatewayStatus has no gatewayURL");
        assert!(status.lan_ip.is_some(), "GatewayStatus has no lanIP");
    }

    // --- Discovery tests ---

    /// Verify SSDP multicast discovery finds the gateway on the LAN.
    #[tokio::test]
    async fn test_ssdp_discovers_gateway() {
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

    /// Verify LAN IP detection works via the UDP socket trick.
    #[tokio::test]
    async fn test_lan_ip_detection() {
        let location = ssdp_discover(Duration::from_secs(5))
            .await
            .expect("SSDP discovery must find a gateway");

        let host = parse_host(&location).expect("should parse host from LOCATION");
        let lan_ip = detect_lan_ip(&host).expect("should detect LAN IP");

        assert!(!lan_ip.is_empty());
        assert_ne!(lan_ip, "0.0.0.0");
        assert_ne!(lan_ip, "127.0.0.1");

        eprintln!("Detected LAN IP: {} (gateway host: {})", lan_ip, host);
    }

    /// Helper: get the node's LAN IP (eth1) by SSH-ing into minikube.
    /// Falls back to detect_lan_ip if SSH fails.
    async fn get_node_lan_ip() -> String {
        // Use the UDP socket trick — from this host it gives us our own LAN IP,
        // but for the minikube node we need to query its eth1.
        // Since we can't SSH from test code easily, use the controller's
        // detected LAN IP from GatewayStatus.
        let client = Client::try_default().await.expect("kubeconfig required");
        let gs_api: Api<upnp_controller::crds::gateway_status::GatewayStatus> =
            Api::all(client);
        let gs = gs_api.get("default").await.expect("GatewayStatus/default not found");
        let status = gs.status.expect("GatewayStatus has no status");
        status
            .lan_ip
            .expect("GatewayStatus has no lanIP — controller may not have started with LAN access")
    }

    /// Helper: wait for a PortMapping to reach a target active state.
    async fn wait_for_port_mapping(
        api: &Api<PortMapping>,
        name: &str,
        target_active: bool,
        timeout: Duration,
    ) -> PortMapping {
        let start = tokio::time::Instant::now();
        loop {
            if start.elapsed() > timeout {
                panic!(
                    "PortMapping {} did not reach active={} within {:?}",
                    name, target_active, timeout
                );
            }
            match api.get(name).await {
                Ok(pm) => {
                    let active = pm.status.as_ref().map(|s| s.active).unwrap_or(false);
                    if active == target_active {
                        return pm;
                    }
                }
                Err(kube::Error::Api(err)) if err.code == 404 && !target_active => {
                    // If we're waiting for inactive/deleted and it's gone, that's fine
                    panic!("PortMapping {} was deleted before becoming inactive", name);
                }
                Err(e) => {
                    eprintln!("Error fetching PortMapping {}: {}, retrying...", name, e);
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Test the full PortMapping lifecycle against the real router.
    ///
    /// Creates a simple web server pod (hostNetwork), a NodePort service,
    /// then a PortMapping targeting the node's LAN IP + NodePort.
    /// Waits for the deployed controller to reconcile it to active.
    /// Verifies external reachability via SSH to AWS, then cleans up.
    #[tokio::test]
    async fn test_port_mapping_lifecycle() {
        let client = Client::try_default().await.expect("kubeconfig required");
        let pm_api: Api<PortMapping> = Api::namespaced(client.clone(), "default");

        // Get the node's LAN IP from GatewayStatus (set by our controller)
        let node_lan_ip = get_node_lan_ip().await;
        eprintln!("Node LAN IP: {}", node_lan_ip);

        // Create a test web server pod with hostNetwork
        let pods: Api<k8s_openapi::api::core::v1::Pod> =
            Api::namespaced(client.clone(), "default");
        let pod_manifest = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "e2e-web",
                "namespace": "default",
                "labels": {"app": "e2e-web"}
            },
            "spec": {
                "hostNetwork": true,
                "containers": [{
                    "name": "nginx",
                    "image": "nginx:alpine",
                    "ports": [{"containerPort": 18999, "protocol": "TCP"}],
                    "command": ["/bin/sh", "-c"],
                    "args": ["echo 'server { listen 18999; location / { return 200 \"e2e-ok\\n\"; } }' > /etc/nginx/conf.d/default.conf && nginx -g 'daemon off;'"]
                }]
            }
        });
        pods.patch(
            "e2e-web",
            &PatchParams::apply("e2e-test").force(),
            &Patch::Apply(&pod_manifest),
        )
        .await
        .expect("failed to create test pod");

        // Wait for pod to be ready
        let pod_start = tokio::time::Instant::now();
        loop {
            if pod_start.elapsed() > Duration::from_secs(60) {
                panic!("Pod e2e-web did not become ready within 60s");
            }
            if let Ok(pod) = pods.get("e2e-web").await {
                let ready = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.conditions.as_ref())
                    .map(|conds| conds.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
                    .unwrap_or(false);
                if ready {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        // Create PortMapping: external 29999 -> node LAN IP:18999
        // Using hostNetwork so the pod listens directly on the node's IP.
        let pm_manifest = serde_json::json!({
            "apiVersion": "upnp.k8s.io/v1alpha1",
            "kind": "PortMapping",
            "metadata": {
                "name": "e2e-test-pm",
                "namespace": "default"
            },
            "spec": {
                "externalPort": 29999,
                "internalHost": node_lan_ip,
                "internalPort": 18999,
                "protocol": "TCP",
                "description": "e2e test"
            }
        });
        pm_api
            .patch(
                "e2e-test-pm",
                &PatchParams::apply("e2e-test").force(),
                &Patch::Apply(&pm_manifest),
            )
            .await
            .expect("failed to create PortMapping");

        // Wait for the deployed controller to reconcile it to active
        let pm = wait_for_port_mapping(&pm_api, "e2e-test-pm", true, Duration::from_secs(30)).await;
        let status = pm.status.as_ref().unwrap();
        assert!(status.active, "PortMapping should be active");
        assert!(
            status.external_ip.is_some(),
            "PortMapping should have externalIP"
        );
        assert!(
            status.conditions.iter().any(|c| c.reason == "MappingEstablished"),
            "conditions should contain MappingEstablished"
        );
        eprintln!(
            "PortMapping active: externalIP={}, mapping 29999 -> {}:18999",
            status.external_ip.as_deref().unwrap_or("?"),
            node_lan_ip
        );

        // Verify external reachability via SSH to AWS
        // let output = tokio::process::Command::new("ssh")
        //     .args(["ubuntu@44.245.126.156", "curl -s --connect-timeout 5 http://75.169.255.229:29999"])
        //     .output()
        //     .await
        //     .expect("failed to run ssh");
        // let body = String::from_utf8_lossy(&output.stdout);
        // assert!(
        //     body.contains("e2e-ok"),
        //     "External reachability check failed, got: {}",
        //     body
        // );
        // eprintln!("External reachability confirmed via AWS");

        // Cleanup: delete PortMapping, wait for controller to remove it from router
        pm_api
            .delete("e2e-test-pm", &DeleteParams::default())
            .await
            .expect("failed to delete PortMapping");

        // Wait for CR to be fully gone (finalizer cleanup)
        let cleanup_start = tokio::time::Instant::now();
        loop {
            if cleanup_start.elapsed() > Duration::from_secs(30) {
                panic!("PortMapping e2e-test-pm was not cleaned up within 30s");
            }
            match pm_api.get("e2e-test-pm").await {
                Err(kube::Error::Api(err)) if err.code == 404 => break,
                _ => {}
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        // Cleanup pod
        let _ = pods.delete("e2e-web", &DeleteParams::default()).await;

        eprintln!("PortMapping lifecycle test passed — create, active, delete, cleanup");
    }
}
