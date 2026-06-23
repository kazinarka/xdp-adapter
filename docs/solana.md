# XDP / AF_XDP in a Solana validator + MEV stack

This note explains *where* kernel-bypass networking earns its keep on Solana,
and how the pieces in this repo map onto a production deployment.

## Solana is a UDP-heavy system

Almost all of Solana's hot-path traffic is UDP, which is exactly what AF_XDP
accelerates:

| Plane | Protocol | Direction | Why it's hot |
|-------|----------|-----------|--------------|
| **TPU** (Transaction Processing Unit) | QUIC (over UDP) | ingress to leader | Clients submit txs; under load this is a flood. |
| **TVU / Turbine** | UDP (shreds) | ingress to all validators | Block data is fanned out as ~1280-byte shreds at very high packet rates. |
| **Repair** | UDP | bidirectional | Fetch missing shreds. |
| **Gossip** | UDP | bidirectional | Cluster membership / CRDS. |

The validator's packet-receive path (the `streamer` in Agave) is a major CPU
consumer, and during spam events the kernel softirq path has historically been a
bottleneck that contributed to cluster slowdowns. That is the pain AF_XDP and
in-kernel XDP filtering target.

## Four concrete use-cases

### 1. Zero-copy shred ingest (validator TVU / MEV "ShredStream")

A validator — or an MEV service that mirrors shreds (Jito's ShredStream is the
well-known example) — wants to see block data as early and as cheaply as
possible. `examples/udp_rx.rs` is the skeleton: bind AF_XDP to the queue(s)
carrying Turbine traffic and parse each datagram with `packet::parse_udp`. The
payload is a shred; the next step (not in this repo) is deshredding → entry →
transaction decode. Earliest userspace visibility with minimal jitter is the
edge an arb/liquidation bot is buying.

### 2. In-kernel DoS filtering (`XDP_DROP`)

This is arguably the highest-value, lowest-risk use of XDP for a validator. An
eBPF program attached at the driver can **drop** junk **before** the kernel even
allocates an `sk_buff`:

- drop packets to ports you don't serve,
- rate-limit per source IP,
- drop malformed / oversized datagrams,
- pass everything else to the normal stack with `XDP_PASS`.

This sheds spam at the cheapest possible point and protects the validator's CPU
during floods. (This repo uses `xsk-rs`, which relies on libbpf's *built-in*
redirect program; authoring a *custom* filter program is the `aya` roadmap item
in the README.)

### 3. Low-latency transaction submission (TX fast path)

`examples/udp_tx.rs` is the shape of a submit path that bypasses the kernel UDP
stack to shave syscall + stack-traversal latency when racing a transaction into
a leader's slot. Caveat: **TPU ingress is QUIC now**, so a complete submitter
needs a userspace QUIC stack (e.g. `quinn`) layered on top of the AF_XDP
datagram transport — AF_XDP moves the UDP packets; QUIC gives you the TPU
session. For plain UDP endpoints (some relays, older paths) the example works
as-is.

### 4. Tap / mirror for analytics

Run an XDP program that `XDP_PASS`es traffic to the validator *and* clones
interesting packets to an AF_XDP socket for an out-of-band MEV/analytics
pipeline, without touching the validator process.

## Production deployment notes

- **NIC support.** Native XDP (best perf) needs a supporting driver: `mlx5`
  (Mellanox/NVIDIA), `ice`/`i40e`/`ixgbe` (Intel), and `ena` (AWS Nitro). On
  bare-metal validators you'll usually have one of these. `veth` (this repo's
  test rig) and unsupported NICs fall back to **generic/SKB mode** — correct but
  slower; fine for testing, not for line rate.
- **Multi-queue scaling.** One `XdpSocket` binds to one `(interface, queue)`.
  To use more than one core, configure RSS (`ethtool -X` / `-N`) to steer
  Solana's port range across N queues, create one `XdpSocket` per queue, and pin
  each to the CPU that handles that queue's IRQ. This crate is intentionally
  single-queue-per-socket to make that pattern explicit.
- **Don't starve the kernel.** When you bind AF_XDP to a queue, you divert that
  queue's traffic away from the normal stack. Keep management traffic (SSH, RPC,
  metrics) on a different queue/IP, or use an XDP program that only redirects
  Solana's dynamic UDP port range (commonly `8000–10000`) and `XDP_PASS`es the
  rest.
- **No kernel = you do ARP.** Because TX emits raw L2 frames, you must supply the
  next-hop MAC (see `UdpFrame::dst_mac`). In production, resolve the gateway's
  MAC once at startup (read the neighbor table / send one ARP) and cache it;
  don't do it per packet.
- **Checksum offload.** This crate computes IPv4 + UDP checksums in software,
  which is always correct. If your NIC + bind flags support TX checksum offload
  you can later skip the UDP checksum computation for a small CPU win.
- **Tuning knobs** worth measuring: `frame_count` (more = more burst tolerance),
  RX/TX batch sizes, `poll_timeout_ms`, busy-polling
  (`SO_PREFER_BUSY_POLL` / `SO_BUSY_POLL_BUDGET`), and CPU/IRQ pinning. The
  defaults in `XdpConfig` are sane starting points, not tuned for any specific
  NIC.

## Where this repo stops (and a real validator integration begins)

This crate gives you a tested transport + codec. A full integration would add:
deshredding/entry decode for ingest, a `quinn` QUIC layer for TPU submit, a
custom `aya` XDP program for filtering, and per-queue/per-core orchestration.
Those are deliberately out of scope so the transport stays small, auditable, and
reusable across both the validator and the MEV-bot services.
