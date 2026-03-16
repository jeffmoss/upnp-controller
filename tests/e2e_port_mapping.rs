//! E2E tests: controller smoke tests, SSDP discovery, PortMapping lifecycle,
//! and annotation-driven port forwarding.
//!
//! All tests run against a real k3s cluster with macvtap LAN access and a real router.
//! The deployed controller handles reconciliation — tests create CRs and verify status.
//!
//! Requires:
//! - A running k3s cluster (KVM + macvtap) with CRDs installed and controller deployed
//! - `just e2e` (or `KUBECONFIG=k3s/kubeconfig cargo test --features e2e`)

#[cfg(feature = "e2e")]
mod tests {
    use std::time::Duration;

    use k8s_openapi::api::apps::v1::Deployment;
    use k8s_openapi::api::core::v1::Service;
    use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
    use kube::api::{Api, DeleteParams, Patch, PatchParams};
    use kube::Client;

    use upnp_controller::config::{detect_lan_ip, parse_host};
    use upnp_controller::crds::gateway_status::GatewayStatus;
    use upnp_controller::crds::port_mapping::PortMapping;
    use upnp_controller::upnp::discovery::ssdp_discover;

    // --- Controller smoke tests ---

    #[tokio::test]
    async fn test_crds_installed() {
        let client = Client::try_default().await.expect("kubeconfig required");
        let crds: Api<CustomResourceDefinition> = Api::all(client);

        let pm = crds.get("portmappings.upnp-controller.io").await;
        assert!(pm.is_ok(), "PortMapping CRD not found");

        let gs = crds.get("gatewaystatuses.upnp-controller.io").await;
        assert!(gs.is_ok(), "GatewayStatus CRD not found");
    }

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

    #[tokio::test]
    async fn test_ssdp_discovers_gateway() {
        let location = ssdp_discover(Duration::from_secs(5)).await;
        assert!(location.is_some(), "SSDP discovery should find a gateway on the LAN");
        let url = location.unwrap();
        assert!(url.starts_with("http://"), "SSDP LOCATION should be an HTTP URL, got: {}", url);
        eprintln!("SSDP discovered gateway: {}", url);
    }

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

    // --- Helpers ---

    /// Assert the cluster has at least 2 nodes and return the controller's node name.
    /// Test pods use anti-affinity against this to ensure cross-node traffic.
    async fn get_controller_node(client: &Client) -> String {
        // Verify multi-node cluster
        let nodes: Api<k8s_openapi::api::core::v1::Node> = Api::all(client.clone());
        let node_list = nodes.list(&Default::default()).await.expect("failed to list nodes");
        assert!(
            node_list.items.len() >= 2,
            "need at least 2 nodes for cross-node tests, found {}",
            node_list.items.len()
        );

        // Find controller's node
        let pods: Api<k8s_openapi::api::core::v1::Pod> =
            Api::namespaced(client.clone(), "upnp-controller");
        let controller_pod = pods
            .list(&Default::default())
            .await
            .expect("failed to list controller pods")
            .items
            .into_iter()
            .find(|p| p.metadata.name.as_deref().unwrap_or("").starts_with("upnp-controller"))
            .expect("controller pod not found");
        let node = controller_pod
            .spec
            .as_ref()
            .and_then(|s| s.node_name.as_deref())
            .expect("controller pod has no nodeName")
            .to_string();
        eprintln!("Controller on node: {}, cluster has {} nodes", node, node_list.items.len());
        node
    }

    /// Build a pod anti-affinity spec that avoids the given node.
    fn anti_affinity_for_node(node_name: &str) -> serde_json::Value {
        serde_json::json!({
            "nodeAffinity": {
                "requiredDuringSchedulingIgnoredDuringExecution": {
                    "nodeSelectorTerms": [{
                        "matchExpressions": [{
                            "key": "kubernetes.io/hostname",
                            "operator": "NotIn",
                            "values": [node_name]
                        }]
                    }]
                }
            }
        })
    }

    async fn get_external_ip() -> String {
        let client = Client::try_default().await.expect("kubeconfig required");
        let api: Api<GatewayStatus> = Api::all(client);
        let gs = api.get("default").await.expect("GatewayStatus/default not found");
        gs.status.unwrap().external_ip.expect("no externalIP")
    }

    async fn get_controller_lan_ip() -> String {
        let client = Client::try_default().await.expect("kubeconfig required");
        let api: Api<GatewayStatus> = Api::all(client);
        let gs = api.get("default").await.expect("GatewayStatus/default not found");
        gs.status.unwrap().lan_ip.expect("no lanIP")
    }

    async fn wait_for_port_mapping(
        api: &Api<PortMapping>,
        name: &str,
        target_active: bool,
        timeout: Duration,
    ) -> PortMapping {
        let start = tokio::time::Instant::now();
        loop {
            if start.elapsed() > timeout {
                panic!("PortMapping {} did not reach active={} within {:?}", name, target_active, timeout);
            }
            match api.get(name).await {
                Ok(pm) => {
                    let active = pm.status.as_ref().map(|s| s.active).unwrap_or(false);
                    if active == target_active {
                        return pm;
                    }
                }
                Err(kube::Error::Api(err)) if err.code == 404 && !target_active => {
                    panic!("PortMapping {} was deleted before becoming inactive", name);
                }
                Err(e) => {
                    eprintln!("Error fetching PortMapping {}: {}, retrying...", name, e);
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn wait_for_port_mapping_exists(
        api: &Api<PortMapping>,
        name: &str,
        timeout: Duration,
    ) -> PortMapping {
        let start = tokio::time::Instant::now();
        loop {
            if start.elapsed() > timeout {
                panic!("PortMapping {} did not appear within {:?}", name, timeout);
            }
            if let Ok(pm) = api.get(name).await {
                return pm;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    // --- PortMapping CRD lifecycle test ---

    /// Test the PortMapping CRD lifecycle: create → active → external reachability → delete.
    /// Pod runs on the controller's node (hostNetwork), PortMapping targets the controller's LAN IP.
    #[tokio::test]
    async fn test_port_mapping_lifecycle() {
        let client = Client::try_default().await.expect("kubeconfig required");
        let pods: Api<k8s_openapi::api::core::v1::Pod> = Api::namespaced(client.clone(), "default");
        let pm_api: Api<PortMapping> = Api::namespaced(client.clone(), "default");

        let node_lan_ip = get_controller_lan_ip().await;
        let controller_node = get_controller_node(&client).await;
        eprintln!("Controller LAN IP: {}, node: {}", node_lan_ip, controller_node);

        // Cleanup leftovers
        let _ = pods.delete("e2e-pm-web", &DeleteParams::default()).await;
        let _ = pm_api.delete("e2e-pm-mapping", &DeleteParams::default()).await;
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Create hostNetwork pod on the controller's node
        pods.create(&Default::default(), &serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": "e2e-pm-web", "labels": {"app": "e2e-pm-web"} },
            "spec": {
                "hostNetwork": true,
                "nodeSelector": { "kubernetes.io/hostname": controller_node },
                "containers": [{
                    "name": "nginx", "image": "nginx:alpine",
                    "ports": [{"containerPort": 18999}],
                    "command": ["/bin/sh", "-c"],
                    "args": ["echo 'server { listen 18999; location / { return 200 \"e2e-pm-ok\\n\"; } }' > /etc/nginx/conf.d/default.conf && nginx -g 'daemon off;'"]
                }]
            }
        })).unwrap()).await.expect("failed to create pod");

        // Wait for ready
        let start = tokio::time::Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(30) { panic!("pod not ready in 30s"); }
            if let Ok(p) = pods.get("e2e-pm-web").await {
                if p.status.as_ref().and_then(|s| s.conditions.as_ref())
                    .map(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
                    .unwrap_or(false) { break; }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        // Create PortMapping
        pm_api.patch("e2e-pm-mapping", &PatchParams::apply("e2e-test").force(), &Patch::Apply(serde_json::json!({
            "apiVersion": "upnp-controller.io/v1alpha1",
            "kind": "PortMapping",
            "metadata": { "name": "e2e-pm-mapping", "namespace": "default" },
            "spec": {
                "externalPort": 29999, "internalHost": node_lan_ip,
                "internalPort": 18999, "protocol": "TCP", "description": "e2e lifecycle"
            }
        }))).await.expect("failed to create PortMapping");

        let pm = wait_for_port_mapping(&pm_api, "e2e-pm-mapping", true, Duration::from_secs(30)).await;
        let status = pm.status.as_ref().unwrap();
        assert!(status.active);
        assert!(status.conditions.iter().any(|c| c.reason == "MappingEstablished"));
        let external_ip = status.external_ip.as_deref().unwrap();
        eprintln!("PortMapping active: {}:29999 -> {}:18999", external_ip, node_lan_ip);

        // External reachability via AWS
        let output = tokio::process::Command::new("ssh")
            .args(["-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=10",
                "ubuntu@44.245.126.156",
                &format!("curl -s --connect-timeout 5 http://{}:29999", external_ip)])
            .output().await.expect("ssh failed");
        let body = String::from_utf8_lossy(&output.stdout);
        assert!(body.contains("e2e-pm-ok"), "External check failed: {}", body);
        eprintln!("External reachability confirmed");

        // Cleanup
        let _ = pm_api.delete("e2e-pm-mapping", &DeleteParams::default()).await;
        let start = tokio::time::Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(30) { panic!("cleanup timeout"); }
            if pm_api.get("e2e-pm-mapping").await.is_err() { break; }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let _ = pods.delete("e2e-pm-web", &DeleteParams::default()).await;
        eprintln!("PortMapping lifecycle test passed");
    }

    // --- Annotation-driven port forwarding test (cross-node) ---

    /// Test annotation port-forwarding with pods on different nodes from the controller.
    /// Uses the TCP proxy to bridge traffic across the cluster.
    #[tokio::test]
    async fn test_annotation_port_forward() {
        let client = Client::try_default().await.expect("kubeconfig required");
        let deploy_api: Api<Deployment> = Api::namespaced(client.clone(), "default");
        let svc_api: Api<Service> = Api::namespaced(client.clone(), "default");
        let pm_api: Api<PortMapping> = Api::namespaced(client.clone(), "default");

        let external_ip = get_external_ip().await;
        let controller_node = get_controller_node(&client).await;

        // Cleanup leftovers
        let _ = svc_api.delete("e2e-ann-svc", &DeleteParams::default()).await;
        let _ = deploy_api.delete("e2e-ann-hello", &DeleteParams::default()).await;
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Deployment: 2 replicas, scheduled AWAY from the controller node
        let affinity = anti_affinity_for_node(&controller_node);
        deploy_api.patch("e2e-ann-hello", &PatchParams::apply("e2e-test").force(), &Patch::Apply(serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": { "name": "e2e-ann-hello" },
            "spec": {
                "replicas": 2,
                "selector": { "matchLabels": { "app": "e2e-ann-hello" } },
                "template": {
                    "metadata": { "labels": { "app": "e2e-ann-hello" } },
                    "spec": {
                        "affinity": affinity,
                        "containers": [{
                            "name": "nginx", "image": "nginx:alpine",
                            "ports": [{ "containerPort": 80 }],
                            "command": ["/bin/sh", "-c"],
                            "args": ["HOSTNAME=$(hostname); echo \"server { listen 80; location / { return 200 \\\"hello from $HOSTNAME\\n\\\"; } }\" > /etc/nginx/conf.d/default.conf && nginx -g 'daemon off;'"]
                        }]
                    }
                }
            }
        }))).await.expect("failed to create deployment");

        // Annotated ClusterIP service
        svc_api.patch("e2e-ann-svc", &PatchParams::apply("e2e-test").force(), &Patch::Apply(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {
                "name": "e2e-ann-svc",
                "annotations": { "upnp-controller.io/port-forward": "28080:80" }
            },
            "spec": {
                "type": "ClusterIP",
                "selector": { "app": "e2e-ann-hello" },
                "ports": [{ "port": 80, "targetPort": 80, "protocol": "TCP" }]
            }
        }))).await.expect("failed to create service");

        // Wait for deployment
        let start = tokio::time::Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(60) { panic!("deployment not ready in 60s"); }
            if let Ok(d) = deploy_api.get("e2e-ann-hello").await {
                if d.status.as_ref().and_then(|s| s.available_replicas).unwrap_or(0) >= 2 { break; }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        // Wait for auto-created PortMapping
        let pm_name = "default-e2e-ann-svc-28080-tcp";
        wait_for_port_mapping_exists(&pm_api, pm_name, Duration::from_secs(15)).await;
        let pm = wait_for_port_mapping(&pm_api, pm_name, true, Duration::from_secs(30)).await;
        assert!(pm.status.as_ref().unwrap().active);
        eprintln!("PortMapping active via annotation: ext:28080 -> proxy -> ClusterIP:80 (cross-node)");

        // External reachability via AWS
        let output = tokio::process::Command::new("ssh")
            .args(["-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=10",
                "ubuntu@44.245.126.156",
                &format!("curl -s --connect-timeout 5 http://{}:28080", external_ip)])
            .output().await.expect("ssh failed");
        let body = String::from_utf8_lossy(&output.stdout);
        assert!(body.contains("hello from"), "External check failed: {}", body);
        eprintln!("Annotation port-forward reachable externally: {}", body.trim());

        // Remove annotation → PortMapping should be cleaned up
        svc_api.patch("e2e-ann-svc", &PatchParams::apply("e2e-test").force(), &Patch::Apply(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": { "name": "e2e-ann-svc", "annotations": {} },
            "spec": {
                "type": "ClusterIP",
                "selector": { "app": "e2e-ann-hello" },
                "ports": [{ "port": 80, "targetPort": 80, "protocol": "TCP" }]
            }
        }))).await.expect("failed to remove annotation");

        let start = tokio::time::Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(30) { panic!("PortMapping cleanup timeout"); }
            if pm_api.get(pm_name).await.is_err() { break; }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        eprintln!("PortMapping cleaned up after annotation removal");

        let _ = svc_api.delete("e2e-ann-svc", &DeleteParams::default()).await;
        let _ = deploy_api.delete("e2e-ann-hello", &DeleteParams::default()).await;
        eprintln!("Annotation port-forward test passed");
    }
}
