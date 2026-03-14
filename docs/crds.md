# CRD Reference

This document provides a complete reference for the Custom Resource Definitions used by upnp-controller.

## PortMapping

**API group**: `upnp.k8s.io/v1alpha1`
**Scope**: Namespaced
**Short name**: `pm`

A `PortMapping` represents a single UPnP port forwarding rule on the router. Creating one tells the controller to call `AddPortMapping` on the router; deleting one triggers `DeletePortMapping`.

### Full example

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

### Spec fields

| Field | Type | Required | Constraints | Description |
|-------|------|----------|-------------|-------------|
| `externalPort` | integer | yes | 1-65535 | Port on the router's WAN interface that will accept incoming traffic |
| `internalHost` | string | yes | -- | LAN IP address (or hostname) of the target machine |
| `internalPort` | integer | yes | 1-65535 | Port on the internal host to forward traffic to |
| `protocol` | string | yes | `TCP` or `UDP` | Transport protocol for the mapping |
| `description` | string | no | -- | Human-readable description stored in the router's mapping table. Defaults to the CR name if omitted |

### Status fields

Status is managed entirely by the controller. Do not edit it manually.

| Field | Type | Description |
|-------|------|-------------|
| `active` | boolean | `true` when the port mapping is confirmed active on the router |
| `externalIP` | string | The router's current WAN IP address (populated on successful add) |
| `leaseExpiry` | date-time | When the current lease expires. The controller renews 30 seconds before this time |
| `lastRenewal` | date-time | Timestamp of the last successful `AddPortMapping` call |
| `conditions` | array | Standard Kubernetes-style conditions (see below) |

### Conditions

| Type | Status | Reason | Meaning |
|------|--------|--------|---------|
| `Active` | `True` | `MappingEstablished` | The port mapping is live on the router |
| `Active` | `False` | `AddFailed` | The last `AddPortMapping` attempt failed. Check `message` for details |

### Printer columns

`kubectl get portmappings` shows:

```
NAME        EXTERNAL PORT   INTERNAL HOST    PROTOCOL   ACTIVE   EXTERNAL IP
minecraft   25565           192.168.0.50     TCP        true     75.169.255.229
```

### Finalizer

The controller adds the finalizer `upnp.k8s.io/cleanup` to every `PortMapping`. On deletion, it calls `DeletePortMapping` on the router before allowing the resource to be garbage collected. If the router has already expired the mapping, the finalizer still completes successfully.

---

## GatewayStatus

**API group**: `upnp.k8s.io/v1alpha1`
**Scope**: Cluster
**Singleton**: There is exactly one instance, named `default`

`GatewayStatus` is a read-only resource that provides visibility into the controller's connection to the router. It is created automatically on startup if it does not exist.

### Full example

```yaml
apiVersion: upnp.k8s.io/v1alpha1
kind: GatewayStatus
metadata:
  name: default
spec: {}
status:
  externalIP: "75.169.255.229"
  gatewayURL: "http://192.168.0.1:5000/rootDesc.xml"
  subscriptionID: "uuid:12345678-abcd-1234-abcd-123456789abc"
  subscriptionExpiry: "2026-03-14T15:30:00Z"
  lastSeen: "2026-03-14T15:00:00Z"
  ready: true
```

### Status fields

| Field | Type | Description |
|-------|------|-------------|
| `externalIP` | string | The router's current WAN IP address |
| `gatewayURL` | string | The `rootDesc.xml` URL used for UPnP discovery |
| `subscriptionID` | string | The GENA subscription SID, if active |
| `subscriptionExpiry` | date-time | When the GENA subscription will expire (renewal happens at half the TTL) |
| `lastSeen` | date-time | Last time the controller successfully communicated with the router |
| `ready` | boolean | `true` when an external IP is known. Use this as a health signal |

### Printer columns

```
NAME      EXTERNAL IP       READY   LAST SEEN
default   75.169.255.229    true    2026-03-14T15:00:00Z
```

---

## Examples

### Forward HTTP traffic to a web server

```yaml
apiVersion: upnp.k8s.io/v1alpha1
kind: PortMapping
metadata:
  name: web-http
  namespace: default
spec:
  externalPort: 80
  internalHost: "192.168.0.100"
  internalPort: 8080
  protocol: TCP
  description: "Web server HTTP"
---
apiVersion: upnp.k8s.io/v1alpha1
kind: PortMapping
metadata:
  name: web-https
  namespace: default
spec:
  externalPort: 443
  internalHost: "192.168.0.100"
  internalPort: 8443
  protocol: TCP
  description: "Web server HTTPS"
```

### Forward a game server with TCP and UDP

```yaml
apiVersion: upnp.k8s.io/v1alpha1
kind: PortMapping
metadata:
  name: game-tcp
  namespace: gaming
spec:
  externalPort: 27015
  internalHost: "192.168.0.200"
  internalPort: 27015
  protocol: TCP
  description: "Game server TCP"
---
apiVersion: upnp.k8s.io/v1alpha1
kind: PortMapping
metadata:
  name: game-udp
  namespace: gaming
spec:
  externalPort: 27015
  internalHost: "192.168.0.200"
  internalPort: 27015
  protocol: UDP
  description: "Game server UDP"
```

### Forward SSH access

```yaml
apiVersion: upnp.k8s.io/v1alpha1
kind: PortMapping
metadata:
  name: ssh
  namespace: default
spec:
  externalPort: 2222
  internalHost: "192.168.0.10"
  internalPort: 22
  protocol: TCP
  description: "SSH access"
```

### Check gateway health

```bash
# View the gateway status
kubectl get gatewaystatus

# Get detailed status
kubectl get gatewaystatus default -o yaml

# Watch for WAN IP changes
kubectl get gatewaystatus -w
```
