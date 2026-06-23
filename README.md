# rxdp

Production-oriented **AF_XDP** (XDP socket) helpers in Rust for high-performance
UDP ingest/egress — built with **Solana validator + MEV** workloads in mind.

It pairs a thin, safe wrapper over [`xsk-rs`](https://crates.io/crates/xsk-rs)
(itself a binding to `libbpf`'s `AF_XDP` machinery) with a dependency-free
Ethernet/IPv4/UDP codec, so the protocol logic is portable and unit-tested
independently of the Linux-only transport.

## Why AF_XDP

A normal UDP socket copies every packet through the kernel network stack and
costs a syscall per `recvmsg`/`sendmsg`. AF_XDP lets an in-driver eBPF program
`XDP_REDIRECT` packets straight into a shared-memory region (**UMEM**) that your
process reads **with no copy and no per-packet syscall**. For a validator
ingesting Turbine shreds at hundreds of thousands of packets/sec, or an MEV bot
racing a transaction to a leader, that saved latency and CPU is the whole game.

### The four rings (the core mental model)

| Ring         | Producer | Consumer | Meaning                            |
|--------------|----------|----------|------------------------------------|
| `FILL`       | us       | kernel   | "here are empty frames to fill"    |
| `RX`         | kernel   | us       | "here are frames I filled for you" |
| `TX`         | us       | kernel   | "please transmit these frames"     |
| `COMPLETION` | kernel   | us       | "I'm done with these TX frames"    |

**Invariant you must uphold:** every UMEM frame is on at most one ring (or in
your hand) at a time. `rxdp` enforces this by splitting the frame pool into
disjoint RX and TX sets — see `src/socket.rs`.

## Layout

| Path | Platform | What |
|------|----------|------|
| `src/packet.rs` | all | Eth/IPv4/UDP parse + build + checksums. Unit-tested everywhere. |
| `src/config.rs` | all | Validated `XdpConfig`. Unit-tested everywhere. |
| `src/socket.rs` | **Linux** | `XdpSocket`: UMEM + 4-ring management, `recv_batch`/`send`. |
| `examples/udp_rx.rs` | Linux | Zero-copy UDP capture ("shred listener" skeleton). |
| `examples/udp_tx.rs` | Linux | Craft + blast UDP ("fast submit" skeleton). |
| `tests/loopback.rs` | Linux | veth round-trip integration test (`#[ignore]`, needs root). |
| `docs/solana.md` | — | Where XDP fits in a validator / MEV stack. |

## Build & test

The pure core builds and tests on **any** OS (handy on a macOS laptop):

```sh
cargo test          # 15 unit tests for the packet codec + config
```

The transport and examples build on **Linux** only. System prerequisites:

```sh
# Debian/Ubuntu
sudo apt-get install -y clang llvm libbpf-dev libelf-dev zlib1g-dev linux-headers-$(uname -r)
```

```sh
cargo build --release
```

### Run the end-to-end test (Linux, root)

```sh
make veth                 # create veth0 <-> veth1 with IPs + offloads off
make test-integration     # sudo cargo test --test loopback -- --ignored
make veth-down            # tear it down
```

### Try the examples (Linux, root)

```sh
# Terminal 1 — listen on veth1
sudo ./target/release/examples/udp_rx veth1

# Terminal 2 — send 1000 datagrams from veth0 to veth1
DST_MAC=$(cat /sys/class/net/veth1/address)
sudo ./target/release/examples/udp_tx veth0 $DST_MAC 10.11.0.2 8001 1000
```

## Status / roadmap

- [x] Pure Eth/IPv4/UDP codec with checksums (tested).
- [x] Validated config + RX/TX pool split.
- [x] `XdpSocket` RX/TX with frame-ownership management.
- [x] veth integration test.
- [ ] Verified first compile of `src/socket.rs` against `xsk-rs 0.8` on Linux
      (a couple of accessor names — `Data::contents`, cursor write — may need a
      small tweak; the surrounding logic is the stable part).
- [ ] Batched TX (`send_batch`) + busy-poll / `SO_PREFER_BUSY_POLL` tuning.
- [ ] Multi-queue scaling: one `XdpSocket` per NIC queue, pinned to its IRQ CPU.
- [ ] Optional custom XDP/eBPF program (via `aya`) for in-kernel pre-filtering.

See `docs/solana.md` for how this maps onto a validator / MEV deployment.
