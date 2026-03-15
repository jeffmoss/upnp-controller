# === Build ===

build:
    cargo build

build-release:
    cargo build --release

test:
    cargo test

# === Image ===

# Build container image into the active minikube profile's Docker daemon
build-image profile="minikube":
    eval $(minikube -p {{profile}} docker-env) && \
    docker build -t upnp-controller:latest .

# === Cluster management ===

cluster-up-docker:
    minikube start -p upnp-test-docker --driver=docker

# Ensure the lan-direct macvtap network exists in libvirt.
# Gives the KVM VM a real IP on your physical LAN via DHCP.
lan-network:
    virsh net-info lan-direct >/dev/null 2>&1 || virsh net-define lan-direct.xml
    virsh net-start lan-direct 2>/dev/null || true

# Requires libvirt/qemu-kvm:
#   sudo apt install qemu-kvm libvirt-daemon-system
#   sudo usermod -aG libvirt $USER
# Minikube auto-installs the kvm2 driver on first use.
cluster-up-kvm: lan-network
    minikube start -p upnp-test-kvm --driver=kvm2 --kvm-network=lan-direct

cluster-down profile:
    minikube delete -p {{profile}}

# Deploy CRDs + controller to the current context
deploy:
    kubectl apply -k config/default

# Wait for controller pod to be ready (timeout 120s)
wait-ready:
    kubectl -n upnp-controller wait --for=condition=available \
        deployment/upnp-controller --timeout=120s

# === E2E testing ===

# Run e2e tests against current kubeconfig context (manual mode)
e2e:
    cargo test --features e2e

# One-command: Docker driver test cycle
# If any step fails, cluster stays up for debugging.
e2e-docker: cluster-up-docker
    just build-image upnp-test-docker
    kubectl config use-context upnp-test-docker
    just deploy
    just wait-ready
    just e2e
    just cluster-down upnp-test-docker

# One-command: KVM driver test cycle (requires libvirt/qemu-kvm)
# If any step fails, cluster stays up for debugging.
e2e-kvm: cluster-up-kvm
    just build-image upnp-test-kvm
    kubectl config use-context upnp-test-kvm
    just deploy
    just wait-ready
    cargo test --features e2e_kvm
    just cluster-down upnp-test-kvm

# === Dev cluster (LAN-routable KVM) ===

# Start a dev cluster on your physical LAN for manual testing
dev-up: lan-network
    minikube start -p upnp-dev --driver=kvm2 --kvm-network=lan-direct
    just build-image upnp-dev
    kubectl config use-context upnp-dev
    just deploy
    just wait-ready

dev-down:
    just cluster-down upnp-dev

# === Utilities ===

logs:
    kubectl -n upnp-controller logs -f deploy/upnp-controller

status:
    @minikube profile list 2>/dev/null || true
    @echo "---"
    @kubectl -n upnp-controller get pods 2>/dev/null || true
    @echo "---"
    @kubectl get gatewaystatus 2>/dev/null || true
