# Deployment Guide

## Prerequisites

- **Kubernetes 1.25+** (for CRD v1 and kube-rs compatibility)
- **UPnP-enabled router** with IGD (Internet Gateway Device) support
- **hostNetwork access** -- the controller pod must be on the same L2 network as the router to send/receive UPnP traffic
- **kubectl** and optionally **kustomize** installed locally

## Quick deploy with kustomize

The `config/` directory follows the [kubebuilder](https://book.kubebuilder.io/) layout:

```bash
# Review what will be applied
kubectl kustomize config/default

# Apply everything: CRDs, namespace, RBAC, and deployment
kubectl apply -k config/default
```

This creates:
- `upnp-controller` namespace
- `PortMapping` and `GatewayStatus` CRDs
- ServiceAccount, ClusterRole, and ClusterRoleBinding
- A single-replica Deployment with `hostNetwork: true`

### Customizing the gateway URL

Edit `config/manager/manager.yaml` and set the `GATEWAY_URL` environment variable to your router's `rootDesc.xml` URL:

```yaml
env:
  - name: GATEWAY_URL
    value: "http://192.168.1.1:5000/rootDesc.xml"
```

Common router URLs:
- Most routers: `http://192.168.0.1:5000/rootDesc.xml`
- Some Netgear: `http://192.168.1.1:5000/rootDesc.xml`
- Some TP-Link: `http://192.168.0.1:1900/rootDesc.xml`

If unsure, try discovering your router:

```bash
# From a machine on the same network
curl -s http://192.168.0.1:5000/rootDesc.xml | head -20
```

## Manual step-by-step deployment

### 1. Install CRDs

```bash
kubectl apply -k config/crd
```

Verify:

```bash
kubectl get crd portmappings.upnp.k8s.io
kubectl get crd gatewaystatuses.upnp.k8s.io
```

### 2. Create namespace and RBAC

```bash
kubectl apply -k config/rbac
```

This creates:
- `upnp-controller` namespace
- `upnp-controller` ServiceAccount
- `upnp-controller` ClusterRole with permissions for PortMappings, GatewayStatuses, Nodes, Leases, and Events
- ClusterRoleBinding linking the two

### 3. Deploy the controller

```bash
kubectl apply -k config/manager
```

### 4. Verify

```bash
kubectl -n upnp-controller get pods
kubectl -n upnp-controller logs -f deploy/upnp-controller
```

You should see log lines indicating successful gateway discovery and GENA subscription.

## Minikube development setup

For local development, minikube needs to reach your physical router. This requires the VM or container to be on the host network.

### With Docker driver

```bash
minikube start --driver=docker --network=host
```

### Build and load the image

```bash
# Build the image
docker build -t upnp-controller:dev .

# Load into minikube
minikube image load upnp-controller:dev
```

### Deploy with the dev tag

```bash
# Update the image tag in kustomization
cd config/manager
kustomize edit set image upnp-controller=upnp-controller:dev
kubectl apply -k ../default
```

### Run outside the cluster

For faster iteration, run the controller directly on your workstation:

```bash
# Install CRDs
kubectl apply -k config/crd

# Run locally with your kubeconfig
GATEWAY_URL="http://192.168.0.1:5000/rootDesc.xml" \
LOG_LEVEL=debug \
POD_IP=$(hostname -I | awk '{print $1}') \
NODE_NAME=$(kubectl get nodes -o jsonpath='{.items[0].metadata.name}') \
cargo run
```

## Verifying the deployment

### 1. Check the controller is running

```bash
kubectl -n upnp-controller get pods
```

Expected: one pod in `Running` state.

### 2. Check GatewayStatus

```bash
kubectl get gatewaystatus
```

Expected:

```
NAME      EXTERNAL IP      READY   LAST SEEN
default   75.169.255.229   true    2026-03-14T15:00:00Z
```

If `READY` is `false`, check the controller logs for gateway discovery errors.

### 3. Create a test PortMapping

```bash
cat <<EOF | kubectl apply -f -
apiVersion: upnp.k8s.io/v1alpha1
kind: PortMapping
metadata:
  name: test-mapping
  namespace: default
spec:
  externalPort: 19999
  internalHost: "192.168.0.1"
  internalPort: 19999
  protocol: TCP
  description: "deployment test"
EOF
```

```bash
kubectl get portmappings
```

Expected: `ACTIVE` is `true` within a few seconds.

### 4. Clean up the test

```bash
kubectl delete portmapping test-mapping
```

### 5. Check metrics

```bash
kubectl -n upnp-controller port-forward deploy/upnp-controller 9090:9090
curl http://localhost:9090/metrics
```

## Troubleshooting

### Pod is running but GatewayStatus shows ready=false

- The router may not support UPnP IGD, or UPnP may be disabled in router settings
- The `GATEWAY_URL` may be wrong -- check with `curl` from the node
- The pod needs `hostNetwork: true` to reach the router

### "Failed to discover gateway services"

- Verify the router's rootDesc.xml is reachable: `curl http://<router-ip>:5000/rootDesc.xml`
- Some routers use a different port (1900, 49152, etc.)
- Check that the rootDesc.xml contains a `WANIPConnection` or `WANPPPConnection` service

### "GENA subscribe failed: falling back to polling"

- This is non-fatal. The controller will poll every `POLL_INTERVAL_SECS` instead
- Some routers do not support GENA eventing
- Ensure the router can reach the pod's NOTIFY port (9091 by default)

### Port mapping shows active=false with AddFailed

- Check the condition message: `kubectl get pm <name> -o jsonpath='{.status.conditions[0].message}'`
- Common causes: port already in use by another mapping, router rejected the request
- Some routers limit the number of concurrent port mappings

### Node annotation is not being set

- Ensure `ANNOTATE_NODES=true` (the default)
- Ensure `NODE_NAME` is set (usually via the downward API)
- Check RBAC: the controller needs `patch` on `nodes`

### High memory or CPU usage

- The controller is designed to be lightweight (32 MiB RAM, minimal CPU)
- If memory grows, check for a large number of PortMapping CRs being reconciled frequently
- Increase `POLL_INTERVAL_SECS` if polling is too aggressive

## Upgrading

### Updating the controller image

```bash
# Build and push the new image, then:
kubectl -n upnp-controller set image deployment/upnp-controller controller=upnp-controller:v0.2.0
```

Or update `config/manager/kustomization.yaml`:

```yaml
images:
  - name: upnp-controller
    newTag: v0.2.0
```

Then:

```bash
kubectl apply -k config/default
```

### Updating CRDs

CRDs can be updated in place:

```bash
kubectl apply -k config/crd
```

Existing CRs are preserved. New fields are added with their defaults. The controller will reconcile all existing resources on restart.

### Rolling back

The Deployment uses `strategy: Recreate` (only one instance should run at a time to avoid duplicate port mappings). To roll back:

```bash
kubectl -n upnp-controller rollout undo deployment/upnp-controller
```

Port mappings on the router persist independently of the controller. If the controller is down, mappings will expire at their lease expiry time (default: 1 hour).
