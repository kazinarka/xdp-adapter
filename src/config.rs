//! Validated configuration for an [`crate::socket::XdpSocket`].
//!
//! Everything here is pure (no Linux dependency) so it can be constructed and
//! validated in unit tests on any platform.

use crate::error::ConfigError;

/// How the UMEM frame pool is split. AF_XDP gives you one flat array of frames;
/// in practice you dedicate some to the RX path (kept cycling through the FILL
/// ring) and some to the TX path (a free list you draw from when sending).
///
/// We split the pool 50/50 by default, which is a sane starting point for a
/// mixed ingest+egress service. A pure shred-listener could shift this heavily
/// toward RX; a pure submitter toward TX.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolSplit {
    pub rx_frames: usize,
    pub tx_frames: usize,
}

/// Configuration for binding an AF_XDP socket to a single NIC queue.
///
/// AF_XDP sockets bind to **one (interface, queue_id) pair**. To use multiple
/// hardware queues (the usual way to scale past one core) you create one
/// `XdpSocket` per queue, typically pinned to the CPU that handles that queue's
/// IRQ. RSS/`ethtool -X` steers flows across queues.
#[derive(Clone, Debug)]
pub struct XdpConfig {
    /// Interface name, e.g. `"eth0"` or `"veth1"`.
    pub if_name: String,
    /// Hardware queue id to bind to. Start with 0.
    pub queue_id: u32,
    /// Total number of UMEM frames. Must be a power of two, even, and >= 64.
    /// More frames = more in-flight packets tolerated before drops.
    pub frame_count: usize,
    /// Per-frame size in bytes. 2048 fits a standard 1500-MTU packet with
    /// headroom; use 4096 if you enable jumbo frames.
    pub frame_size: u32,
    /// Max frames pulled from the RX ring per `recv_batch` call.
    pub rx_batch_size: usize,
    /// Max frames pushed to the TX ring per `send_batch` call.
    pub tx_batch_size: usize,
    /// Timeout passed to `poll()` inside `recv_batch`, in milliseconds.
    /// `0` = non-blocking poll, `-1` = block indefinitely.
    pub poll_timeout_ms: i32,
}

impl Default for XdpConfig {
    fn default() -> Self {
        Self {
            if_name: String::new(),
            queue_id: 0,
            frame_count: 4096,
            frame_size: 2048,
            rx_batch_size: 64,
            tx_batch_size: 64,
            poll_timeout_ms: 100,
        }
    }
}

impl XdpConfig {
    /// Build a config for `if_name` with otherwise-default tuning.
    pub fn new(if_name: impl Into<String>) -> Self {
        Self {
            if_name: if_name.into(),
            ..Self::default()
        }
    }

    /// Validate the configuration, returning the RX/TX pool split that the
    /// socket layer will use. Call this at startup so misconfiguration fails
    /// fast and loudly rather than corrupting the ring state at runtime.
    pub fn validate(&self) -> std::result::Result<PoolSplit, ConfigError> {
        if self.if_name.is_empty() {
            return Err(ConfigError::EmptyInterface);
        }
        if self.frame_size != 2048 && self.frame_size != 4096 {
            return Err(ConfigError::BadFrameSize(self.frame_size));
        }
        if self.frame_count < 64 || !self.frame_count.is_power_of_two() {
            return Err(ConfigError::BadFrameCount(self.frame_count));
        }
        if self.frame_count % 2 != 0 {
            return Err(ConfigError::OddFrameCount(self.frame_count));
        }

        let rx_frames = self.frame_count / 2;
        let tx_frames = self.frame_count - rx_frames;

        // The RX batch can't exceed the RX pool, and likewise for TX. If it
        // did, we'd try to hand the kernel more frames than we own.
        if self.rx_batch_size > rx_frames || self.tx_batch_size > tx_frames {
            return Err(ConfigError::BatchTooLarge {
                rx: self.rx_batch_size,
                frames: self.frame_count,
            });
        }

        Ok(PoolSplit {
            rx_frames,
            tx_frames,
        })
    }

    /// Usable payload capacity per frame: the frame size minus the space we
    /// reserve for Ethernet+IPv4+UDP headers. This is the largest UDP payload
    /// `send` can accept in a single (non-fragmented) datagram.
    pub fn max_udp_payload(&self) -> usize {
        use crate::packet::{ETH_HDR_LEN, IPV4_HDR_LEN, UDP_HDR_LEN};
        self.frame_size as usize - (ETH_HDR_LEN + IPV4_HDR_LEN + UDP_HDR_LEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid_with_interface() {
        let cfg = XdpConfig::new("eth0");
        let split = cfg.validate().expect("default config should validate");
        assert_eq!(split.rx_frames, 2048);
        assert_eq!(split.tx_frames, 2048);
    }

    #[test]
    fn empty_interface_rejected() {
        let cfg = XdpConfig::default();
        assert_eq!(cfg.validate(), Err(ConfigError::EmptyInterface));
    }

    #[test]
    fn non_power_of_two_frame_count_rejected() {
        let cfg = XdpConfig {
            frame_count: 3000,
            ..XdpConfig::new("eth0")
        };
        assert_eq!(cfg.validate(), Err(ConfigError::BadFrameCount(3000)));
    }

    #[test]
    fn bad_frame_size_rejected() {
        let cfg = XdpConfig {
            frame_size: 1500,
            ..XdpConfig::new("eth0")
        };
        assert_eq!(cfg.validate(), Err(ConfigError::BadFrameSize(1500)));
    }

    #[test]
    fn oversized_batch_rejected() {
        let cfg = XdpConfig {
            frame_count: 64,
            rx_batch_size: 100,
            ..XdpConfig::new("eth0")
        };
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::BatchTooLarge { .. })
        ));
    }

    #[test]
    fn max_udp_payload_accounts_for_headers() {
        let cfg = XdpConfig::new("eth0"); // 2048 frame
                                          // 2048 - (14 + 20 + 8) = 2006
        assert_eq!(cfg.max_udp_payload(), 2006);
    }
}
