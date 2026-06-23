//! # rxdp — AF_XDP helpers in Rust
//!
//! A small, production-oriented wrapper over [`xsk-rs`] (a safe binding to
//! `libbpf`'s `AF_XDP` machinery) plus a dependency-free Ethernet/IPv4/UDP
//! codec. The crate targets the workloads a Solana validator / MEV service
//! cares about: **line-rate, zero-copy UDP ingest** (Turbine shreds, repair,
//! gossip) and **low-latency UDP egress** (fast transaction submission to a
//! leader's TPU).
//!
//! ## Mental model: AF_XDP in one paragraph
//!
//! An XDP/eBPF program runs *in the NIC driver*, before the kernel builds an
//! `sk_buff`. It can `XDP_REDIRECT` a packet into an **XSK map**, landing it in
//! a shared-memory region called the **UMEM**. Userspace then reads that packet
//! with **no copy** and **no per-packet syscall**, coordinating with the kernel
//! through four single-producer/single-consumer rings:
//!
//! | Ring         | Producer | Consumer | Meaning                              |
//! |--------------|----------|----------|--------------------------------------|
//! | `FILL`       | us       | kernel   | "here are empty frames to fill"      |
//! | `RX`         | kernel   | us       | "here are frames I filled for you"   |
//! | `TX`         | us       | kernel   | "please transmit these frames"       |
//! | `COMPLETION` | kernel   | us       | "I'm done with these TX frames"      |
//!
//! Mastering *who produces and who consumes each ring* is most of AF_XDP.
//!
//! ## Layering
//!
//! - [`packet`] — pure, allocation-light Ethernet/IPv4/UDP parse + build +
//!   checksums. Compiles and is unit-tested on every platform.
//! - [`config`] — validated [`XdpConfig`]. Pure, testable.
//! - [`socket`] — the [`socket::XdpSocket`] transport (UMEM + ring management).
//!   **Linux only.**

pub mod config;
pub mod error;
pub mod packet;

#[cfg(target_os = "linux")]
pub mod socket;

pub use config::XdpConfig;
pub use error::{ConfigError, ParseError, Result, XdpError};

#[cfg(target_os = "linux")]
pub use socket::XdpSocket;
