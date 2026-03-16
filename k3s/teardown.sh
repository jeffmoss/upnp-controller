#!/usr/bin/env bash
# Destroy the k3s cluster VMs.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
NODES=(k3s-server k3s-agent-1 k3s-agent-2)

for name in "${NODES[@]}"; do
    if virsh dominfo "$name" &>/dev/null; then
        echo "Destroying $name..."
        virsh destroy "$name" 2>/dev/null || true
        virsh undefine "$name" --remove-all-storage 2>/dev/null || true
    fi
done

# Clean up mgmt network (only if no other VMs use it)
if virsh net-info k3s-mgmt &>/dev/null; then
    virsh net-destroy k3s-mgmt 2>/dev/null || true
    virsh net-undefine k3s-mgmt 2>/dev/null || true
fi

# Clean up generated files
rm -f "$SCRIPT_DIR"/*-rendered.yaml
rm -f "$SCRIPT_DIR"/*.qcow2
rm -f "$SCRIPT_DIR"/*.iso
rm -f "$SCRIPT_DIR"/kubeconfig

echo "k3s cluster destroyed"
