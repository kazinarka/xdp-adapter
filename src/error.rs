//! Error types for the crate.
//!
//! We use [`thiserror`] to derive `std::error::Error` so callers can use `?`
//! and integrate with `anyhow`/`eyre` without friction. Errors are split so a
//! caller can distinguish *configuration* mistakes (their fault, fail fast at
//! startup) from *runtime* transport errors (recoverable, may retry).

use thiserror::Error;

/// Errors that can occur while validating an [`crate::XdpConfig`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("interface name must not be empty")]
    EmptyInterface,

    #[error("frame_count must be a power of two and >= 64, got {0}")]
    BadFrameCount(usize),

    #[error("frame_size must be one of 2048 or 4096, got {0}")]
    BadFrameSize(u32),

    #[error("rx_batch_size ({rx}) + tx reserve must fit within frame_count ({frames})")]
    BatchTooLarge { rx: usize, frames: usize },

    #[error("frame_count ({0}) must be even so it can be split between RX and TX pools")]
    OddFrameCount(usize),
}

/// Errors that occur while parsing a received frame.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("frame too short: needed {needed} bytes, got {got}")]
    TooShort { needed: usize, got: usize },

    #[error("not an IPv4 ethertype (0x{0:04x})")]
    NotIpv4(u16),

    #[error("IPv4 protocol is not UDP (proto {0})")]
    NotUdp(u8),

    #[error("IPv4 IHL {0} is invalid (< 5 words)")]
    BadIhl(u8),
}

/// Top-level error type returned by socket operations.
#[derive(Debug, Error)]
pub enum XdpError {
    #[error("invalid configuration: {0}")]
    Config(#[from] ConfigError),

    /// Wraps a lower-level I/O / libbpf error from the AF_XDP layer.
    #[error("AF_XDP transport error: {0}")]
    Transport(String),

    #[error("UMEM frame pool exhausted (no free TX frames)")]
    NoFreeFrames,

    #[error("payload of {payload} bytes exceeds usable frame capacity of {capacity}")]
    PayloadTooLarge { payload: usize, capacity: usize },
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, XdpError>;
