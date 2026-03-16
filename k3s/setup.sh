#!/usr/bin/env bash
# Create a 3-node k3s cluster on the LAN using KVM.
# 1 server + 2 agents, all on the lan-direct macvtap network.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# Ubuntu LTS codename (update when new LTS releases)
CODENAME="noble"
BASE_IMG="$SCRIPT_DIR/${CODENAME}-minimal-cloudimg-amd64.img"
BASE_IMG_URL="https://cloud-images.ubuntu.com/minimal/daily/${CODENAME}/current/${CODENAME}-minimal-cloudimg-amd64.img"
SSH_PUB_KEY="$(cat ~/.ssh/id_ed25519.pub 2>/dev/null || cat ~/.ssh/id_rsa.pub 2>/dev/null)"
NODES=(k3s-server k3s-agent-1 k3s-agent-2)
VCPUS=2
RAM=2048  # MB
DISK=10   # GB

# --- Download base image if needed ---
if [ ! -f "$BASE_IMG" ]; then
    echo "Downloading Ubuntu 24.04 minimal cloud image..."
    wget -q --show-progress -O "$BASE_IMG" "$BASE_IMG_URL"
fi

# --- Ensure networks are active ---
virsh net-info lan-direct >/dev/null 2>&1 || virsh net-define "$SCRIPT_DIR/../lan-direct.xml"
virsh net-start lan-direct 2>/dev/null || true

virsh net-info k3s-mgmt >/dev/null 2>&1 || virsh net-define "$SCRIPT_DIR/mgmt-network.xml"
virsh net-start k3s-mgmt 2>/dev/null || true

# --- Helpers ---

create_vm() {
    local name=$1
    local cloud_init=$2
    local disk="$SCRIPT_DIR/${name}.qcow2"

    if virsh dominfo "$name" &>/dev/null; then
        echo "$name already exists, skipping"
        return
    fi

    echo "Creating $name..."
    qemu-img create -f qcow2 -b "$BASE_IMG" -F qcow2 "$disk" "${DISK}G"

    # Create cloud-init seed ISO with network config for both NICs
    local seed="$SCRIPT_DIR/${name}-seed.iso"
    local netcfg="$SCRIPT_DIR/${name}-network-config.yaml"
    cat > "$netcfg" <<NETEOF
version: 2
ethernets:
  enp1s0:
    dhcp4: true
  enp2s0:
    dhcp4: true
    dhcp4-overrides:
      use-routes: false
      use-dns: false
NETEOF
    cloud-localds -N "$netcfg" "$seed" "$cloud_init"

    # NIC1 (enp1s0): LAN via macvtap — primary, used by k8s
    # NIC2 (enp2s0): host-only mgmt — for SSH from host
    virt-install \
        --name "$name" \
        --memory "$RAM" \
        --vcpus "$VCPUS" \
        --disk "$disk" \
        --disk "$seed",readonly=on,serial=cidata \
        --os-variant ubuntu24.04 \
        --network network=lan-direct,model=virtio \
        --network network=k3s-mgmt,model=virtio \
        --noautoconsole \
        --import
}

render_server_config() {
    sed "s|\${SSH_PUB_KEY}|$SSH_PUB_KEY|" "$SCRIPT_DIR/cloud-init-server.yaml" \
        > "$SCRIPT_DIR/cloud-init-server-rendered.yaml"
}

render_agent_config() {
    local hostname=$1 server_ip=$2 token=$3
    sed -e "s|\${SSH_PUB_KEY}|$SSH_PUB_KEY|" \
        -e "s|\${HOSTNAME}|$hostname|" \
        -e "s|\${SERVER_IP}|$server_ip|" \
        -e "s|\${K3S_TOKEN}|$token|" \
        "$SCRIPT_DIR/cloud-init-agent.yaml" \
        > "$SCRIPT_DIR/cloud-init-${hostname}-rendered.yaml"
}

get_vm_mgmt_ip() {
    local name=$1 max_wait=45 elapsed=0
    echo -n "Waiting for $name mgmt IP" >&2
    while true; do
        local ip
        ip=$(virsh domifaddr "$name" --source lease 2>/dev/null \
            | grep -oP '10\.44\.0\.\d+' | head -1) || true
        if [ -n "$ip" ]; then
            echo " $ip" >&2
            echo "$ip"
            return
        fi
        echo -n "." >&2
        sleep 1
        elapsed=$((elapsed + 1))
        if [ $elapsed -ge $max_wait ]; then
            echo " TIMEOUT" >&2
            return 1
        fi
    done
}

wait_for_ssh() {
    local ip=$1 max_wait=15 elapsed=0
    echo -n "Waiting for SSH on $ip"
    while ! ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=1 k3s@"$ip" true 2>/dev/null; do
        echo -n "."
        sleep 1
        elapsed=$((elapsed + 1))
        if [ $elapsed -ge $max_wait ]; then
            echo " TIMEOUT"
            return 1
        fi
    done
    echo " ready"
}

wait_for_k3s() {
    local ip=$1 max_wait=60 elapsed=0
    echo -n "Waiting for k3s on $ip"
    while ! ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null k3s@"$ip" "sudo k3s kubectl get node" &>/dev/null; do
        echo -n "."
        sleep 2
        elapsed=$((elapsed + 2))
        if [ $elapsed -ge $max_wait ]; then
            echo " TIMEOUT"
            return 1
        fi
    done
    echo " ready"
}

# === Main ===

echo "=== Setting up k3s cluster ==="

# Server
render_server_config
create_vm k3s-server "$SCRIPT_DIR/cloud-init-server-rendered.yaml"

SERVER_MGMT_IP=$(get_vm_mgmt_ip k3s-server)
wait_for_ssh "$SERVER_MGMT_IP"

SERVER_LAN_IP=$(ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null k3s@"$SERVER_MGMT_IP" \
    "ip -4 addr show enp1s0 2>/dev/null | grep -oP '192\.168\.0\.\d+' | head -1")
echo "k3s-server LAN: $SERVER_LAN_IP  Mgmt: $SERVER_MGMT_IP"

wait_for_k3s "$SERVER_MGMT_IP"
K3S_TOKEN=$(ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null k3s@"$SERVER_MGMT_IP" "sudo cat /var/lib/rancher/k3s/server/node-token")
echo "k3s token obtained"

# Agents
for agent in k3s-agent-1 k3s-agent-2; do
    render_agent_config "$agent" "$SERVER_LAN_IP" "$K3S_TOKEN"
    create_vm "$agent" "$SCRIPT_DIR/cloud-init-${agent}-rendered.yaml"
done

for agent in k3s-agent-1 k3s-agent-2; do
    AGENT_MGMT_IP=$(get_vm_mgmt_ip "$agent")
    wait_for_ssh "$AGENT_MGMT_IP"
    echo "$agent LAN: $(ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null k3s@"$AGENT_MGMT_IP" \
        "ip -4 addr show enp1s0 2>/dev/null | grep -oP '192\.168\.0\.\d+' | head -1")  Mgmt: $AGENT_MGMT_IP"
done

# Fetch kubeconfig
ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null k3s@"$SERVER_MGMT_IP" "sudo cat /etc/rancher/k3s/k3s.yaml" \
    | sed "s/127.0.0.1/$SERVER_MGMT_IP/" \
    > "$SCRIPT_DIR/kubeconfig"
chmod 600 "$SCRIPT_DIR/kubeconfig"

echo ""
echo "=== k3s cluster ready ==="
echo "Kubeconfig: $SCRIPT_DIR/kubeconfig"
echo "  export KUBECONFIG=$SCRIPT_DIR/kubeconfig"
