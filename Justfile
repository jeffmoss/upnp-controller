# === Build ===

build:
    cargo build

build-release:
    cargo build --release

test:
    cargo test

# === k3s cluster ===

# Create a 3-node k3s cluster on the LAN (KVM + macvtap)
cluster-up:
    ./k3s/setup.sh

# Destroy the k3s cluster
cluster-down:
    ./k3s/teardown.sh

# === Image ===

# Build and import container image into the k3s cluster
build-image:
    docker build -t upnp-controller:latest .
    docker save upnp-controller:latest | gzip > /tmp/upnp-controller.tar.gz
    KUBECONFIG=k3s/kubeconfig kubectl get nodes -o jsonpath='{range .items[*]}{.status.addresses[?(@.type=="InternalIP")].address}{"\n"}{end}' \
        | while read ip; do \
            echo "Importing image to $$ip..."; \
            scp -o StrictHostKeyChecking=no /tmp/upnp-controller.tar.gz k3s@$$ip:/tmp/; \
            ssh -o StrictHostKeyChecking=no k3s@$$ip "sudo k3s ctr images import /tmp/upnp-controller.tar.gz && rm /tmp/upnp-controller.tar.gz"; \
        done
    rm -f /tmp/upnp-controller.tar.gz

# === Deploy ===

# Deploy CRDs + controller to the k3s cluster
deploy:
    KUBECONFIG=k3s/kubeconfig kubectl apply -k config/default

# Wait for controller pod to be ready (timeout 120s)
wait-ready:
    KUBECONFIG=k3s/kubeconfig kubectl -n upnp-controller wait --for=condition=available \
        deployment/upnp-controller --timeout=120s

# === E2E testing ===

# Run e2e tests against the k3s cluster
e2e:
    KUBECONFIG=k3s/kubeconfig cargo test --features e2e

# One-command: full e2e test cycle
e2e-full: cluster-up build-image deploy wait-ready e2e

# === Utilities ===

logs:
    KUBECONFIG=k3s/kubeconfig kubectl -n upnp-controller logs -f deploy/upnp-controller

status:
    @KUBECONFIG=k3s/kubeconfig kubectl get nodes 2>/dev/null || true
    @echo "---"
    @KUBECONFIG=k3s/kubeconfig kubectl -n upnp-controller get pods 2>/dev/null || true
    @echo "---"
    @KUBECONFIG=k3s/kubeconfig kubectl get gatewaystatus 2>/dev/null || true

# SSH into a k3s node
ssh node="k3s-server":
    ssh -o StrictHostKeyChecking=no k3s@$(virsh domifaddr {{node}} --source lease | grep -oP '10\.44\.0\.\d+' | head -1)
