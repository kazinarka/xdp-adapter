# Developer tasks for the rxdp AF_XDP crate.
#
# The veth target builds a virtual ethernet pair so the integration test has a
# real L2 link without any physical NIC. AF_XDP runs in generic/SKB mode on
# veth, so no special driver support is needed.

VETH0 ?= veth0
VETH1 ?= veth1
IP0   ?= 10.11.0.1/24
IP1   ?= 10.11.0.2/24

.PHONY: build test test-integration veth veth-down lint fmt

build:
	cargo build --release

# Pure-core unit tests. Run anywhere (Linux/macOS).
test:
	cargo test

# End-to-end AF_XDP test over the veth pair. Requires root + `make veth` first.
test-integration:
	sudo -E cargo test --test loopback -- --ignored --nocapture

# Create veth0 <-> veth1, bring them up, assign IPs, and disable offloads that
# interfere with XDP on virtual devices.
veth:
	sudo ip link add dev $(VETH0) type veth peer name $(VETH1)
	sudo ip link set $(VETH0) up
	sudo ip link set $(VETH1) up
	sudo ip addr add $(IP0) dev $(VETH0)
	sudo ip addr add $(IP1) dev $(VETH1)
	sudo ethtool -K $(VETH0) tx off rx off gro off gso off tso off || true
	sudo ethtool -K $(VETH1) tx off rx off gro off gso off tso off || true
	@echo "veth pair ready: $(VETH0) <-> $(VETH1)"

veth-down:
	-sudo ip link del $(VETH0)
	@echo "veth pair removed"

lint:
	cargo clippy --all-targets -- -D warnings

fmt:
	cargo fmt --all
