# Architecture

This document describes the internal architecture of the upnp-controller, including component interactions, startup sequence, reconciliation state machines, and data flows.

## Overall architecture

```mermaid
graph TB
    subgraph Kubernetes
        API[Kubernetes API Server]
        PM[PortMapping CRDs]
        GS[GatewayStatus CRD]
        Node[Node Objects]
        Lease[Leader Election Lease]
    end

    subgraph "upnp-controller Pod (hostNetwork)"
        Main[main.rs<br/>Startup & wiring]
        PMCtrl[PortMapping Controller<br/>kube-rs reconciler]
        GWCtrl[GatewayStatus Controller<br/>kube-rs reconciler]
        UPnP[UPnP Layer<br/>SOAP client]
        GENA[GENA Eventing<br/>subscribe + renew]
        Poll[Polling Fallback<br/>GetExternalIPAddress]
        AxumNotify[Axum HTTP Server<br/>POST /notify :9091]
        AxumMetrics[Axum HTTP Server<br/>GET /metrics :9090]
        State[EventingState<br/>current_external_ip<br/>last_notify]
    end

    Router[Router / Gateway<br/>UPnP IGD]
    ExtDNS[external-dns]
    Prometheus[Prometheus]

    API --> PM
    API --> GS
    API --> Node
    API --> Lease

    PMCtrl -->|watch + reconcile| PM
    PMCtrl -->|AddPortMapping / DeletePortMapping| UPnP
    GWCtrl -->|watch + reconcile| GS
    GWCtrl -->|patch annotation| Node
    GWCtrl -->|read| State

    UPnP -->|SOAP over HTTP| Router
    GENA -->|SUBSCRIBE / RENEW| Router
    Router -->|NOTIFY POST| AxumNotify
    AxumNotify -->|update| State
    Poll -->|GetExternalIPAddress| UPnP

    ExtDNS -->|read annotation| Node
    Prometheus -->|scrape| AxumMetrics

    Main --> PMCtrl
    Main --> GWCtrl
    Main --> GENA
    Main --> Poll
    Main --> AxumNotify
    Main --> AxumMetrics
```

### Component summary

| Component | Source | Responsibility |
|-----------|--------|----------------|
| **main.rs** | `src/main.rs` | Wires everything together: config, discovery, GENA subscribe, spawn controllers and HTTP servers |
| **PortMapping Controller** | `src/controllers/port_mapping_ctrl.rs` | Watches `PortMapping` CRs, calls `AddPortMapping`/`DeletePortMapping` SOAP actions, manages finalizer and lease renewal |
| **GatewayStatus Controller** | `src/controllers/gateway_ctrl.rs` | Ensures the `GatewayStatus/default` singleton exists, patches its status with current WAN IP and subscription info, annotates nodes |
| **UPnP Client** | `src/upnp/port_mapping.rs` | SOAP client for `AddPortMapping`, `DeletePortMapping`, `GetExternalIPAddress`, and `GetGenericPortMappingEntry` |
| **Discovery** | `src/upnp/discovery.rs` | Fetches and parses `rootDesc.xml` to find WANIPConnection/WANPPPConnection control and event URLs |
| **GENA Eventing** | `src/upnp/eventing.rs` | Subscribes to UPnP GENA events, handles subscription renewal loop, parses NOTIFY bodies |
| **Polling Fallback** | `src/main.rs` (inline task) | Calls `GetExternalIPAddress` every `POLL_INTERVAL_SECS` when GENA is silent or unavailable |
| **Axum NOTIFY server** | `src/main.rs` | Receives `POST /notify` callbacks from the router, updates `EventingState` |
| **Axum Metrics server** | `src/main.rs` | Serves Prometheus metrics on `GET /metrics` |
| **EventingState** | `src/upnp/eventing.rs` | Thread-safe shared state holding `current_external_ip`, `last_notify`, and subscription info |
| **Config** | `src/config.rs` | Reads all configuration from environment variables |
| **Metrics** | `src/metrics.rs` | Prometheus registry with all upnp_* metrics |
| **Node Annotation** | `src/node.rs` | Patches `external-dns.alpha.kubernetes.io/target` on the node for external-dns integration |

## Startup sequence

```mermaid
sequenceDiagram
    participant Main as main.rs
    participant Config as Config
    participant K8s as Kubernetes API
    participant Router as Router/Gateway
    participant GENA as GENA Eventing
    participant PMCtrl as PortMapping Controller
    participant GWCtrl as GatewayStatus Controller
    participant Axum as Axum HTTP Servers

    Main->>Config: Config::from_env()
    Main->>Main: init tracing (JSON, env filter)
    Main->>Main: init Metrics registry
    Main->>K8s: Client::try_default()

    Main->>Router: GET rootDesc.xml
    Router-->>Main: XML response
    Main->>Main: parse WANIPConnection URLs<br/>(control_url, event_url)

    Main->>Router: SOAP GetExternalIPAddress
    Router-->>Main: current WAN IP
    Main->>Main: store in EventingState

    Main->>Router: SUBSCRIBE (GENA)
    Router-->>Main: 200 OK + SID

    Main->>GENA: spawn run_renewal_loop(ttl/2)
    Main->>Main: spawn polling fallback loop

    Main->>PMCtrl: spawn PortMapping controller
    Main->>GWCtrl: spawn GatewayStatus controller
    Note over GWCtrl: ensures GatewayStatus/default exists

    Main->>Axum: bind :9091 (NOTIFY) and :9090 (metrics)
    Axum-->>Main: serving
```

## PortMapping reconcile loop

The PortMapping controller uses a kube-rs finalizer to manage the full lifecycle. The state machine below shows the logical states and transitions:

```mermaid
stateDiagram-v2
    [*] --> Pending: CR created

    Pending --> Adding: reconcile_apply triggered

    Adding --> Active: AddPortMapping succeeds
    Adding --> Failed: AddPortMapping errors

    Failed --> Adding: requeue after 60s

    Active --> Renewing: lease expiry approaching<br/>(within 30s buffer)
    Active --> Deleting: CR deleted<br/>(finalizer cleanup)

    Renewing --> Active: AddPortMapping succeeds<br/>(lease renewed)
    Renewing --> Failed: AddPortMapping errors

    Deleting --> Deleted: DeletePortMapping called<br/>finalizer removed
    Deleted --> [*]
```

### Key implementation details

- **Finalizer**: `upnp.k8s.io/cleanup` -- added on first reconcile, removed after cleanup
- **Lease duration**: default 3600 seconds (1 hour), requested via `NewLeaseDuration` SOAP parameter
- **Renewal buffer**: 30 seconds before expiry, the controller requeues to renew
- **Failure requeue**: 60 seconds on `AddPortMapping` failure
- **Error policy**: 30 second requeue on unhandled controller errors
- **Status conditions**: `Active` condition with `True`/`False` status and reason codes `MappingEstablished` or `AddFailed`

## GENA subscription lifecycle

```mermaid
sequenceDiagram
    participant Controller as upnp-controller
    participant Router as Router/Gateway
    participant State as EventingState

    Controller->>Router: SUBSCRIBE<br/>NT: upnp:event<br/>CALLBACK: <http://pod:9091/notify><br/>TIMEOUT: Second-1800
    Router-->>Controller: 200 OK<br/>SID: uuid:xxxx

    Note over Controller: renewal loop starts (every 900s)

    Router->>Controller: POST /notify<br/>(WAN IP changed)
    Controller->>State: update current_external_ip
    Controller->>State: update last_notify timestamp

    Note over Controller: 900s later...
    Controller->>Router: SUBSCRIBE (renew)<br/>SID: uuid:xxxx<br/>TIMEOUT: Second-1800
    Router-->>Controller: 200 OK

    Note over Controller: if GENA silent >10min...
    Controller->>Router: SOAP GetExternalIPAddress<br/>(polling fallback)
    Router-->>Controller: current WAN IP
```

### Fallback behavior

The polling loop runs independently and checks whether GENA has been silent:

1. If `last_notify` is `None` (never received a NOTIFY) or older than 10 minutes, polling activates
2. If GENA subscription failed entirely at startup, polling is always active
3. Poll interval is configurable via `POLL_INTERVAL_SECS` (default: 300 seconds)

### Renewal

- GENA subscriptions are requested with a 1800 second (30 minute) TTL
- The renewal loop fires at half the TTL (every 900 seconds)
- On renewal failure, the subscription is cleared and the controller falls back to polling

## Data flow: PortMapping CR lifecycle

This diagram shows how a `PortMapping` CR flows through the system from creation to deletion:

```mermaid
graph LR
    subgraph User
        A[kubectl apply PortMapping]
    end

    subgraph "Kubernetes API"
        B[PortMapping CR stored]
        C[PortMapping status patched]
    end

    subgraph "upnp-controller"
        D[PortMapping Controller<br/>watches PortMapping CRs]
        E[reconcile_apply]
        F[reconcile_cleanup]
        G[UPnP Client]
    end

    subgraph Router
        H[UPnP IGD Service]
    end

    A -->|create| B
    B -->|watch event| D
    D -->|new/modified| E
    D -->|deleted with finalizer| F

    E -->|AddPortMapping SOAP| G
    G -->|HTTP POST| H
    H -->|success| G
    G -->|ok| E
    E -->|patch status: active=true| C

    F -->|DeletePortMapping SOAP| G
    G -->|HTTP POST| H
    F -->|remove finalizer| B
```

### Step-by-step flow

1. User creates a `PortMapping` CR via `kubectl apply`
2. The Kubernetes API stores the resource and fires a watch event
3. The PortMapping controller receives the event and runs `reconcile_apply`
4. The controller adds the `upnp.k8s.io/cleanup` finalizer if not present
5. The UPnP client sends an `AddPortMapping` SOAP request to the router
6. On success, the controller patches the CR status with `active: true`, the external IP, lease expiry, and an `Active` condition
7. The controller requeues itself for ~30 seconds before lease expiry to renew
8. On renewal, step 5-7 repeat
9. When the user deletes the CR, the finalizer triggers `reconcile_cleanup`
10. The UPnP client sends a `DeletePortMapping` SOAP request to the router
11. The finalizer is removed, allowing Kubernetes to garbage collect the CR

## Data flow: WAN IP change propagation

```mermaid
graph TD
    subgraph Router
        A[WAN IP changes]
    end

    subgraph "upnp-controller"
        B[GENA NOTIFY received<br/>POST /notify]
        C[Polling detects change<br/>GetExternalIPAddress]
        D[EventingState updated]
        E[GatewayStatus Controller]
        F[Node Annotator]
    end

    subgraph Kubernetes
        G[GatewayStatus/default<br/>status.externalIP updated]
        H[Node annotation<br/>external-dns target updated]
    end

    subgraph External
        I[external-dns picks up<br/>new IP from annotation]
    end

    A -->|NOTIFY| B
    A -->|poll response| C
    B --> D
    C --> D
    D -->|read by| E
    E -->|patch status| G
    E -->|annotate| F
    F -->|patch| H
    H -->|read| I
```

When the router's WAN IP changes:

1. The router sends a GENA NOTIFY to `POST /notify` (or the polling loop detects the change)
2. `EventingState.current_external_ip` is updated; `wan_ip_changes` metric is incremented
3. On its next reconcile (every 60 seconds), the GatewayStatus controller reads the new IP from `EventingState`
4. It patches `GatewayStatus/default` with the new `externalIP` and `lastSeen`
5. If `ANNOTATE_NODES=true`, it patches the node's `external-dns.alpha.kubernetes.io/target` annotation
6. external-dns reads the annotation and updates DNS records accordingly
