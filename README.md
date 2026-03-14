# upnp-controller

A Kubernetes controller that manages UPnP port mappings on your home router via Custom Resource Definitions (CRDs). It replaces annotation-based tools like [holepunch](https://github.com/city-dream/holepunch) with a declarative, CRD-driven approach that gives you `kubectl get portmappings` visibility into your NAT rules.

## Features

- **Declarative port mappings** -- create a `PortMapping` CR and the controller configures your router automatically
- **Automatic lease renewal** -- mappings are renewed before expiry so they never silently drop
- **WAN IP tracking** -- a cluster-scoped `GatewayStatus` singleton tracks your external IP in real time
- **GENA eventing** -- subscribes to UPnP GENA notifications for instant WAN IP change detection, with polling fallback
- **External DNS integration** -- optionally annotates Kubernetes nodes with `external-dns.alpha.kubernetes.io/target` so external-dns can publish your WAN IP
- **Prometheus metrics** -- active mappings, renewal counts, failure counts, WAN IP changes, and GENA subscription health
- **Finalizer-based cleanup** -- deleting a `PortMapping` CR removes the mapping from the router before the resource is garbage collected
- **Lightweight** -- single binary, ~32 MiB RAM, runs with `hostNetwork: true`

## Quick start

### Prerequisites

- Kubernetes 1.25+
- A UPnP-enabled router on the same L2 network as at least one node
- `hostNetwork` access (the pod must reach the router directly)

### Installation

```bash
# Deploy CRDs, RBAC, and the controller
kubectl apply -k config/default
```

### Create a PortMapping

```yaml
apiVersion: upnp.k8s.io/v1alpha1
kind: PortMapping
metadata:
  name: minecraft
  namespace: default
spec:
  externalPort: 25565
  internalHost: "192.168.0.50"
  internalPort: 25565
  protocol: TCP
  description: "Minecraft server"
```

```bash
kubectl apply -f mapping.yaml
kubectl get portmappings
```

```
NAME        EXTERNAL PORT   INTERNAL HOST   PROTOCOL   ACTIVE   EXTERNAL IP
minecraft   25565           192.168.0.50    TCP        true     75.169.255.229
```

## CRD reference

Full CRD documentation with field descriptions and examples is in [docs/crds.md](docs/crds.md).

### PortMapping (namespaced)

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `spec.externalPort` | integer (1-65535) | yes | Port on the router's WAN interface |
| `spec.internalHost` | string | yes | LAN IP address to forward traffic to |
| `spec.internalPort` | integer (1-65535) | yes | Port on the internal host |
| `spec.protocol` | `TCP` or `UDP` | yes | Transport protocol |
| `spec.description` | string | no | Human-readable label stored on the router |

### GatewayStatus (cluster-scoped singleton)

A single `GatewayStatus/default` resource is created automatically. It exposes:

| Status field | Description |
|--------------|-------------|
| `externalIP` | Current WAN IP address |
| `gatewayURL` | The router's rootDesc.xml URL |
| `subscriptionID` | Active GENA subscription SID |
| `subscriptionExpiry` | When the GENA subscription expires |
| `lastSeen` | Last successful contact with the router |
| `ready` | `true` when an external IP is known |

## Configuration

All configuration is via environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `GATEWAY_URL` | `http://192.168.0.1:5000/rootDesc.xml` | URL to the router's UPnP root description XML |
| `NOTIFY_PORT` | `9091` | Port the GENA NOTIFY HTTP server listens on |
| `METRICS_PORT` | `9090` | Port the Prometheus metrics endpoint listens on |
| `ANNOTATE_NODES` | `true` | Set `external-dns.alpha.kubernetes.io/target` on the node |
| `POLL_INTERVAL_SECS` | `300` | Fallback polling interval for `GetExternalIPAddress` (seconds) |
| `LOG_LEVEL` | `warn` | Tracing filter (e.g. `info`, `debug`, `upnp_controller=debug`) |
| `LEADER_ELECTION_NAMESPACE` | `upnp-controller` | Namespace for the leader election Lease object |
| `POD_IP` | -- | Pod IP used to build the GENA callback URL (usually from `status.podIP` downward API) |
| `NODE_NAME` | -- | Node name for annotation (usually from `spec.nodeName` downward API) |

## Prometheus metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `upnp_active_port_mappings` | Gauge | -- | Number of currently active PortMappings |
| `upnp_port_mapping_renewals_total` | Counter | `name` | Successful port mapping add/renewals |
| `upnp_port_mapping_failures_total` | Counter | `name`, `reason` | Failed port mapping attempts |
| `upnp_wan_ip_changes_total` | Counter | -- | Detected WAN IP address changes |
| `upnp_gena_subscription_active` | Gauge | -- | 1 if GENA subscription is live, 0 if polling fallback |
| `upnp_gateway_last_seen_seconds` | Gauge | -- | Unix timestamp of last successful router contact |

Metrics are served at `http://<pod>:9090/metrics` in Prometheus text exposition format.

## Architecture

The controller runs as a single-replica `Deployment` with `hostNetwork: true`. It discovers the router's UPnP services by fetching `rootDesc.xml`, then subscribes to GENA events for real-time WAN IP change notification. Two kube-rs reconcile loops manage `PortMapping` and `GatewayStatus` resources independently.

For detailed architecture documentation and Mermaid diagrams, see [docs/architecture.md](docs/architecture.md).

## Development

### Build

```bash
cargo build
cargo build --release
```

### Test

```bash
cargo test
```

### Run locally

Requires a kubeconfig pointing at a cluster with the CRDs installed:

```bash
# Install CRDs
kubectl apply -k config/crd

# Run the controller
GATEWAY_URL="http://192.168.0.1:5000/rootDesc.xml" \
LOG_LEVEL=debug \
POD_IP=192.168.0.10 \
cargo run
```

### Container image

```bash
docker build -t upnp-controller:latest .
```

## Deployment

See [docs/deployment.md](docs/deployment.md) for complete deployment instructions, including kustomize quick deploy, manual step-by-step setup, minikube development, and troubleshooting.

## License

See [LICENSE](LICENSE) for details.
