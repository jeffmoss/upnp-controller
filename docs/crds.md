# CRD Reference

## PortMapping

**API group**: `upnp-controller.io/v1alpha1`
**Scope**: Namespaced
**Short name**: `pm`

A PortMapping represents a single UPnP port forwarding rule on the router.

### Example

```yaml
apiVersion: upnp-controller.io/v1alpha1
kind: PortMapping
metadata:
  name: minecraft
spec:
  externalPort: 25565
  internalHost: "192.168.0.50"
  internalPort: 25565
  protocol: TCP
  description: "Minecraft server"
```

### Spec

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `externalPort` | integer | yes | Port on the router's WAN interface |
| `internalHost` | string | yes | LAN IP to forward traffic to |
| `internalPort` | integer | yes | Port on the internal host |
| `protocol` | `TCP` or `UDP` | yes | Transport protocol |
| `description` | string | no | Human-readable label stored on the router |

### Status

| Field | Type | Description |
|-------|------|-------------|
| `active` | boolean | `true` when the mapping is confirmed on the router |
| `externalIP` | string | Router's current WAN IP |
| `leaseExpiry` | date-time | When the lease expires (renewed 30s before) |
| `lastRenewal` | date-time | Last successful AddPortMapping call |
| `conditions` | array | `Active=True/MappingEstablished` or `Active=False/AddFailed` |

### Finalizer

`upnp-controller.io/cleanup` — ensures DeletePortMapping is called before the CR is garbage collected.

---

## GatewayStatus

**API group**: `upnp-controller.io/v1alpha1`
**Scope**: Cluster
**Singleton**: `default`

Read-only resource tracking the controller's connection to the router.

### Status

| Field | Type | Description |
|-------|------|-------------|
| `externalIP` | string | Router's current WAN IP |
| `gatewayURL` | string | The rootDesc.xml URL (SSDP-discovered or configured) |
| `lanIP` | string | Controller's detected LAN IP |
| `subscriptionID` | string | Active GENA subscription SID |
| `subscriptionExpiry` | date-time | GENA subscription expiry |
| `lastSeen` | date-time | Last successful router contact |
| `ready` | boolean | `true` when an external IP is known |

---

## DNSEndpoint (external-dns)

**API group**: `externaldns.k8s.io/v1alpha1` (not owned by this controller)

The controller watches DNSEndpoint resources annotated with `upnp-controller.io/managed: "true"` and patches A record targets with the current WAN IP.

### Annotation

| Key | Value | Description |
|-----|-------|-------------|
| `upnp-controller.io/managed` | `"true"` | Enables automatic target patching |

### Example

```yaml
apiVersion: externaldns.k8s.io/v1alpha1
kind: DNSEndpoint
metadata:
  name: home
  annotations:
    upnp-controller.io/managed: "true"
spec:
  endpoints:
  - dnsName: home.example.com
    recordType: A
    recordTTL: 60
    targets: []
```
