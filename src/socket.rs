//! The AF_XDP transport: [`XdpSocket`].
//!
//! **Linux only.** This module is compiled solely on Linux (see the `cfg` gate
//! in `lib.rs`) because `xsk-rs` links `libbpf`.
//!
//! ## The frame-ownership rule that AF_XDP makes you enforce
//!
//! `xsk-rs` does **not** track which UMEM frame is on which ring — that's your
//! job, and getting it wrong corrupts the data path. The single invariant is:
//!
//! > **Every UMEM frame must be on at most one ring (or in your hand) at a time.**
//!
//! We uphold it by partitioning the frame pool into two disjoint sets that
//! never share an address:
//!
//! - **RX frames** live on the FILL ring. The kernel takes one, fills it with a
//!   received packet, and hands it back via the RX ring; we read it and return
//!   it to FILL. The set cycles FILL → RX → FILL forever.
//! - **TX frames** are a free list we own. To send, we pop one, write a packet,
//!   and push it to the TX ring; the kernel transmits it and returns it via the
//!   COMPLETION ring, at which point we push it back onto the free list.

use xsk_rs::{
    config::{FrameSize, SocketConfig, UmemConfig},
    CompQueue, FillQueue, FrameDesc, RxQueue, Socket, TxQueue, Umem,
};

use crate::config::XdpConfig;
use crate::error::{Result, XdpError};
use crate::packet::{UdpDatagram, UdpFrame};

/// A bound AF_XDP socket on one (interface, queue) pair, with managed RX/TX
/// frame pools. Not `Send`/`Sync` by design: keep one per thread/core.
pub struct XdpSocket {
    umem: Umem,
    rx_q: RxQueue,
    tx_q: TxQueue,
    fill_q: FillQueue,
    comp_q: CompQueue,

    /// Scratch + canonical storage for the RX pool. These descriptors cycle
    /// through FILL → RX → FILL. At steady state all of them are on the FILL
    /// ring; `recv_batch` reuses the prefix as the consume target.
    rx_descs: Vec<FrameDesc>,
    /// Free TX frames available for `send`. Drained when sending, refilled by
    /// `reclaim_completions`.
    tx_free: Vec<FrameDesc>,
    /// TX frames handed to the kernel and not yet completed. Reused as the
    /// COMPLETION-ring consume target.
    tx_inflight: Vec<FrameDesc>,

    cfg: XdpConfig,
    /// Per-socket IPv4 identification counter (avoids RNG on the hot path).
    ip_id: u16,
}

impl XdpSocket {
    /// Bind a new AF_XDP socket according to `cfg`.
    ///
    /// On success the FILL ring is pre-charged with the entire RX pool, so the
    /// socket is ready to receive immediately. Requires `CAP_NET_RAW` +
    /// `CAP_NET_ADMIN` (or root) and a recent kernel (5.4+ for the features we
    /// use; SKB/generic mode works on `veth` for testing).
    pub fn bind(cfg: XdpConfig) -> Result<Self> {
        let split = cfg.validate()?;

        let if_name: xsk_rs::config::Interface = cfg
            .if_name
            .parse()
            .map_err(|e| XdpError::Transport(format!("bad interface {:?}: {e:?}", cfg.if_name)))?;

        let umem_config = UmemConfig::builder()
            .frame_size(
                FrameSize::new(cfg.frame_size)
                    .map_err(|e| XdpError::Transport(format!("frame size: {e:?}")))?,
            )
            .build()
            .map_err(|e| XdpError::Transport(format!("umem config: {e:?}")))?;

        // `Umem::new` allocates the shared memory region and returns one
        // FrameDesc per frame. It wants the frame count as a `NonZeroU32`; the
        // count is already validated > 0, but convert explicitly so a bad value
        // surfaces as a transport error rather than a panic. The `false` = don't
        // use huge pages.
        let frames_nz = std::num::NonZeroU32::new(frame_count(&cfg) as u32)
            .ok_or_else(|| XdpError::Transport("frame_count must be non-zero".into()))?;
        let (umem, mut frames) = Umem::new(umem_config, frames_nz, false)
            .map_err(|e| XdpError::Transport(format!("umem alloc: {e:?}")))?;

        let socket_config = SocketConfig::builder().build();

        // SAFETY: we hold `umem` for the socket's whole lifetime, and we never
        // place a frame on more than one ring at a time (see module docs).
        let (tx_q, rx_q, fq_cq) = unsafe {
            Socket::new(socket_config, &umem, &if_name, cfg.queue_id)
                .map_err(|e| XdpError::Transport(format!("socket bind: {e:?}")))?
        };
        let (mut fill_q, comp_q) = fq_cq.ok_or_else(|| {
            XdpError::Transport("kernel did not return FILL/COMPLETION queues".into())
        })?;

        // Split the frame pool: first half RX, second half TX. The two halves
        // reference disjoint UMEM addresses, so they can never alias on a ring.
        let tx_descs = frames.split_off(split.rx_frames);
        let rx_descs = frames; // the remaining first `rx_frames` descriptors

        // Pre-charge the FILL ring with the entire RX pool so we can receive
        // immediately. `produce` returns how many it accepted.
        let mut filled = 0;
        while filled < rx_descs.len() {
            // SAFETY: these descriptors are not on any other ring.
            filled += unsafe { fill_q.produce(&rx_descs[filled..]) };
        }

        Ok(Self {
            umem,
            rx_q,
            tx_q,
            fill_q,
            comp_q,
            rx_descs,
            tx_free: tx_descs,
            tx_inflight: Vec::with_capacity(split.tx_frames),
            cfg,
            ip_id: 0,
        })
    }

    /// Receive a batch of packets, invoking `on_packet` once per received frame
    /// with the raw L2 bytes. Returns the number of packets delivered.
    ///
    /// Frames are returned to the FILL ring before this call returns, so the
    /// borrow handed to `on_packet` must not outlive the callback (the closure
    /// signature enforces this). If you need to keep a packet, copy it out.
    pub fn recv_batch<F>(&mut self, mut on_packet: F) -> Result<usize>
    where
        F: FnMut(&[u8]),
    {
        let batch = self.cfg.rx_batch_size;

        // SAFETY: `rx_descs` holds frames currently on the FILL ring; the
        // kernel writes received-frame descriptors into the slice we pass and
        // returns the count. After this, those entries describe frames we own
        // again (they've left the FILL ring).
        let n = unsafe {
            self.rx_q
                .poll_and_consume(&mut self.rx_descs[..batch], self.cfg.poll_timeout_ms)
                .map_err(|e| XdpError::Transport(format!("rx poll: {e:?}")))?
        };

        for desc in &self.rx_descs[..n] {
            // SAFETY: `desc` was just returned by the RX ring and references a
            // valid, kernel-filled UMEM frame we now own.
            let data = unsafe { self.umem.data(desc) };
            on_packet(data.contents());
        }

        // Return the consumed frames to the FILL ring so the kernel can reuse
        // them. Without this, the FILL ring drains and RX silently stops.
        let mut returned = 0;
        while returned < n {
            // SAFETY: frames are no longer on any ring; we're re-arming FILL.
            returned += unsafe { self.fill_q.produce(&self.rx_descs[returned..n]) };
        }
        Ok(n)
    }

    /// Convenience wrapper around [`recv_batch`](Self::recv_batch) that parses
    /// each frame as UDP and invokes `on_udp` only for valid UDP datagrams.
    /// Non-UDP / malformed frames are silently skipped (count still reflects
    /// total frames received).
    pub fn recv_udp<F>(&mut self, mut on_udp: F) -> Result<usize>
    where
        F: FnMut(UdpDatagram<'_>),
    {
        self.recv_batch(|frame| {
            if let Ok(dg) = crate::packet::parse_udp(frame) {
                on_udp(dg);
            }
        })
    }

    /// Send a single UDP datagram described by `frame`. Returns
    /// [`XdpError::NoFreeFrames`] if the TX pool is exhausted — call
    /// [`reclaim_completions`](Self::reclaim_completions) and retry, or size
    /// `frame_count` higher.
    ///
    /// This issues a syscall wakeup per call via `produce_and_wakeup`. For high
    /// throughput, prefer batching at the call site and calling
    /// [`flush`](Self::flush) once; see `examples/udp_tx.rs`.
    pub fn send(&mut self, frame: &UdpFrame<'_>) -> Result<()> {
        let cap = self.cfg.max_udp_payload();
        if frame.payload.len() > cap {
            return Err(XdpError::PayloadTooLarge {
                payload: frame.payload.len(),
                capacity: cap,
            });
        }

        self.reclaim_completions();
        let mut desc = self.tx_free.pop().ok_or(XdpError::NoFreeFrames)?;

        // Stamp the per-socket IP id before borrowing the UMEM: `data_mut`
        // borrows `self.umem` for the rest of this block, which would conflict
        // with the `&mut self` that `next_ip_id` needs.
        let mut spec = frame.clone();
        spec.ip_id = self.next_ip_id();

        // Write directly into the UMEM frame — zero intermediate allocation.
        // SAFETY: `desc` is a TX-pool frame we own and that is on no ring.
        {
            let mut data = unsafe { self.umem.data_mut(&mut desc) };
            let total = spec.encoded_len();

            // A fresh TX frame has data length 0, so `contents_mut()` would
            // expose an empty slice. The cursor's position field *is* the
            // frame descriptor's data length, so setting it to `total` both
            // grows the writable region to the full encoded size and records
            // the length the kernel will transmit — no separate `set_len`.
            data.cursor().set_pos(total);
            let n = spec.encode_into(data.contents_mut());
            debug_assert_eq!(n, total);
        }

        // Hand the frame to the TX ring and kick the kernel to transmit.
        // SAFETY: frame is now owned by the kernel until it appears on COMP.
        let inflight = std::slice::from_ref(&desc);
        let mut sent = 0;
        while sent < 1 {
            sent = unsafe {
                self.tx_q
                    .produce_and_wakeup(&inflight[sent..])
                    .map_err(|e| XdpError::Transport(format!("tx: {e:?}")))?
            };
        }
        self.tx_inflight.push(desc);
        Ok(())
    }

    /// Kick the kernel to transmit any queued TX frames. `produce_and_wakeup`
    /// already wakes the kernel, so this is mainly a hook for a batched design.
    pub fn flush(&mut self) -> Result<()> {
        self.reclaim_completions();
        Ok(())
    }

    /// Move any completed TX frames from the COMPLETION ring back onto the free
    /// list. Called automatically by [`send`](Self::send); expose it so a busy
    /// loop can reclaim proactively.
    pub fn reclaim_completions(&mut self) -> usize {
        if self.tx_inflight.is_empty() {
            return 0;
        }
        // SAFETY: the COMPLETION ring returns frames the kernel is done with;
        // we move them from `tx_inflight` back to `tx_free`.
        let n = unsafe { self.comp_q.consume(&mut self.tx_inflight) };
        for _ in 0..n {
            if let Some(d) = self.tx_inflight.pop() {
                self.tx_free.push(d);
            }
        }
        n
    }

    /// Number of TX frames currently available to send.
    pub fn free_tx_frames(&self) -> usize {
        self.tx_free.len()
    }

    fn next_ip_id(&mut self) -> u16 {
        let id = self.ip_id;
        self.ip_id = self.ip_id.wrapping_add(1);
        id
    }
}

fn frame_count(cfg: &XdpConfig) -> usize {
    cfg.frame_count
}
