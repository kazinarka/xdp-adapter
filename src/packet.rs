//! Dependency-free Ethernet / IPv4 / UDP parsing and construction.
//!
//! AF_XDP hands you (and takes from you) **raw L2 frames** — there is no kernel
//! socket layer adding headers for you. So to send a UDP datagram you must
//! build the entire Ethernet + IPv4 + UDP header stack yourself, and to receive
//! one you must parse it yourself. That is exactly what this module does.
//!
//! Design choices:
//! - **Zero-copy parsing**: [`parse_udp`] borrows from the input buffer and
//!   returns slices into it — no allocation, suitable for a hot RX loop.
//! - **In-place building**: [`UdpFrame::encode_into`] writes directly into a
//!   caller-provided buffer (i.e. a UMEM frame), so the TX path allocates
//!   nothing per packet. [`UdpFrame::encode`] is a convenience that allocates.
//! - All multi-byte integers are big-endian ("network byte order").

use std::net::Ipv4Addr;

use crate::error::ParseError;

/// Length of an Ethernet II header (no VLAN tag).
pub const ETH_HDR_LEN: usize = 14;
/// Length of an IPv4 header with no options (IHL = 5).
pub const IPV4_HDR_LEN: usize = 20;
/// Length of a UDP header.
pub const UDP_HDR_LEN: usize = 8;
/// Total header overhead for an Ethernet/IPv4/UDP datagram.
pub const HDR_OVERHEAD: usize = ETH_HDR_LEN + IPV4_HDR_LEN + UDP_HDR_LEN;

/// EtherType value indicating the payload is an IPv4 packet.
pub const ETHERTYPE_IPV4: u16 = 0x0800;
/// IPv4 `protocol` field value for UDP.
pub const IPPROTO_UDP: u8 = 17;

/// A 6-byte Ethernet (MAC) address.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    /// The broadcast address `ff:ff:ff:ff:ff:ff`.
    pub const BROADCAST: MacAddr = MacAddr([0xff; 6]);

    pub const fn new(octets: [u8; 6]) -> Self {
        MacAddr(octets)
    }
}

impl std::fmt::Debug for MacAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let b = self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

/// A parsed UDP-over-IPv4-over-Ethernet datagram. Field slices borrow from the
/// frame that was parsed, so this is a zero-copy view.
#[derive(Debug, PartialEq, Eq)]
pub struct UdpDatagram<'a> {
    pub dst_mac: MacAddr,
    pub src_mac: MacAddr,
    pub src_ip: Ipv4Addr,
    pub dst_ip: Ipv4Addr,
    pub src_port: u16,
    pub dst_port: u16,
    /// The UDP payload (application bytes — e.g. a Solana shred or packet).
    pub payload: &'a [u8],
}

/// Parse a raw Ethernet frame as a UDP/IPv4 datagram.
///
/// Returns a borrowed [`UdpDatagram`] on success. Anything that isn't
/// well-formed UDP-over-IPv4 yields a [`ParseError`] (cheaply — this is on the
/// RX hot path, so it does bounds checks and bails, never panics).
pub fn parse_udp(frame: &[u8]) -> std::result::Result<UdpDatagram<'_>, ParseError> {
    // --- Ethernet header ---
    if frame.len() < ETH_HDR_LEN {
        return Err(ParseError::TooShort {
            needed: ETH_HDR_LEN,
            got: frame.len(),
        });
    }
    let dst_mac = MacAddr([frame[0], frame[1], frame[2], frame[3], frame[4], frame[5]]);
    let src_mac = MacAddr([frame[6], frame[7], frame[8], frame[9], frame[10], frame[11]]);
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    if ethertype != ETHERTYPE_IPV4 {
        return Err(ParseError::NotIpv4(ethertype));
    }

    // --- IPv4 header ---
    let ip = &frame[ETH_HDR_LEN..];
    if ip.len() < IPV4_HDR_LEN {
        return Err(ParseError::TooShort {
            needed: ETH_HDR_LEN + IPV4_HDR_LEN,
            got: frame.len(),
        });
    }
    // The low nibble of byte 0 is IHL: header length in 32-bit words.
    let ihl = (ip[0] & 0x0f) as usize;
    if ihl < 5 {
        return Err(ParseError::BadIhl(ihl as u8));
    }
    let ip_hdr_len = ihl * 4;
    if ip[9] != IPPROTO_UDP {
        return Err(ParseError::NotUdp(ip[9]));
    }
    if ip.len() < ip_hdr_len {
        return Err(ParseError::TooShort {
            needed: ETH_HDR_LEN + ip_hdr_len,
            got: frame.len(),
        });
    }
    let src_ip = Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]);
    let dst_ip = Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]);

    // --- UDP header ---
    let udp = &ip[ip_hdr_len..];
    if udp.len() < UDP_HDR_LEN {
        return Err(ParseError::TooShort {
            needed: ETH_HDR_LEN + ip_hdr_len + UDP_HDR_LEN,
            got: frame.len(),
        });
    }
    let src_port = u16::from_be_bytes([udp[0], udp[1]]);
    let dst_port = u16::from_be_bytes([udp[2], udp[3]]);
    let udp_len = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    // The UDP length field covers the 8-byte header + payload. Clamp to what's
    // actually present so a malformed/truncated length can't over-read.
    let payload_len = udp_len.saturating_sub(UDP_HDR_LEN);
    let avail = udp.len() - UDP_HDR_LEN;
    let payload = &udp[UDP_HDR_LEN..UDP_HDR_LEN + payload_len.min(avail)];

    Ok(UdpDatagram {
        dst_mac,
        src_mac,
        src_ip,
        dst_ip,
        src_port,
        dst_port,
        payload,
    })
}

/// Specification for a UDP datagram to transmit. Build one of these, then
/// [`encode_into`](UdpFrame::encode_into) it directly into a UMEM frame.
#[derive(Debug, Clone)]
pub struct UdpFrame<'a> {
    pub dst_mac: MacAddr,
    pub src_mac: MacAddr,
    pub src_ip: Ipv4Addr,
    pub dst_ip: Ipv4Addr,
    pub src_port: u16,
    pub dst_port: u16,
    /// `ttl` for the IPv4 header. 64 is the conventional default.
    pub ttl: u8,
    /// 16-bit IPv4 identification field. For datagrams that won't be
    /// fragmented this can be anything; a per-socket incrementing counter is
    /// fine and avoids RNG on the hot path.
    pub ip_id: u16,
    pub payload: &'a [u8],
}

impl<'a> UdpFrame<'a> {
    /// Total encoded size of this frame (headers + payload).
    pub fn encoded_len(&self) -> usize {
        HDR_OVERHEAD + self.payload.len()
    }

    /// Encode the full Ethernet/IPv4/UDP frame into `buf`, returning the number
    /// of bytes written. `buf` must be at least [`encoded_len`](Self::encoded_len).
    ///
    /// Both the IPv4 header checksum and the UDP checksum are computed. (The
    /// UDP checksum is technically optional over IPv4, but NICs and middleboxes
    /// are happier when it's correct, and it's cheap.)
    pub fn encode_into(&self, buf: &mut [u8]) -> usize {
        let total = self.encoded_len();
        assert!(
            buf.len() >= total,
            "buffer too small: need {total}, have {}",
            buf.len()
        );

        // ----- Ethernet header (14 bytes) -----
        buf[0..6].copy_from_slice(&self.dst_mac.0);
        buf[6..12].copy_from_slice(&self.src_mac.0);
        buf[12..14].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());

        // ----- IPv4 header (20 bytes) -----
        let ip = &mut buf[ETH_HDR_LEN..ETH_HDR_LEN + IPV4_HDR_LEN];
        let ip_total_len = (IPV4_HDR_LEN + UDP_HDR_LEN + self.payload.len()) as u16;
        ip[0] = 0x45; // version 4, IHL 5
        ip[1] = 0x00; // DSCP / ECN
        ip[2..4].copy_from_slice(&ip_total_len.to_be_bytes());
        ip[4..6].copy_from_slice(&self.ip_id.to_be_bytes());
        // Flags = "Don't Fragment" (0x4000). We never emit fragments here.
        ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
        ip[8] = self.ttl;
        ip[9] = IPPROTO_UDP;
        ip[10..12].copy_from_slice(&[0, 0]); // checksum placeholder
        ip[12..16].copy_from_slice(&self.src_ip.octets());
        ip[16..20].copy_from_slice(&self.dst_ip.octets());
        let ip_csum = ipv4_checksum(ip);
        ip[10..12].copy_from_slice(&ip_csum.to_be_bytes());

        // ----- UDP header (8 bytes) + payload -----
        let udp_len = (UDP_HDR_LEN + self.payload.len()) as u16;
        let udp_off = ETH_HDR_LEN + IPV4_HDR_LEN;
        {
            let udp = &mut buf[udp_off..udp_off + UDP_HDR_LEN];
            udp[0..2].copy_from_slice(&self.src_port.to_be_bytes());
            udp[2..4].copy_from_slice(&self.dst_port.to_be_bytes());
            udp[4..6].copy_from_slice(&udp_len.to_be_bytes());
            udp[6..8].copy_from_slice(&[0, 0]); // checksum placeholder
        }
        buf[udp_off + UDP_HDR_LEN..total].copy_from_slice(self.payload);

        // UDP checksum spans a pseudo-header + the UDP header + payload.
        let udp_csum = udp_checksum(self.src_ip, self.dst_ip, &buf[udp_off..total]);
        buf[udp_off + 6..udp_off + 8].copy_from_slice(&udp_csum.to_be_bytes());

        total
    }

    /// Allocate a `Vec<u8>` and encode into it. Convenience for tests and
    /// non-hot paths; prefer [`encode_into`](Self::encode_into) in a TX loop.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = vec![0u8; self.encoded_len()];
        self.encode_into(&mut buf);
        buf
    }
}

/// Compute the IPv4 header checksum over a 20-byte (or longer, with options)
/// header. The checksum field itself must be zero in `header` when called.
pub fn ipv4_checksum(header: &[u8]) -> u16 {
    ones_complement_fold(sum16(header, 0))
}

/// Compute the UDP checksum given the IPv4 pseudo-header fields and the UDP
/// header+payload slice (`udp` = 8-byte header followed by payload, with the
/// checksum field zeroed).
pub fn udp_checksum(src_ip: Ipv4Addr, dst_ip: Ipv4Addr, udp: &[u8]) -> u16 {
    // Pseudo-header: src(4) + dst(4) + zero(1) + proto(1) + udp_len(2).
    let mut sum: u32 = 0;
    let s = src_ip.octets();
    let d = dst_ip.octets();
    sum += u16::from_be_bytes([s[0], s[1]]) as u32;
    sum += u16::from_be_bytes([s[2], s[3]]) as u32;
    sum += u16::from_be_bytes([d[0], d[1]]) as u32;
    sum += u16::from_be_bytes([d[2], d[3]]) as u32;
    sum += IPPROTO_UDP as u32;
    sum += udp.len() as u32;

    let csum = ones_complement_fold(sum16(udp, sum));
    // RFC 768: a computed checksum of zero is transmitted as all-ones, because
    // zero is reserved to mean "no checksum".
    if csum == 0 {
        0xffff
    } else {
        csum
    }
}

/// Sum `data` as a sequence of big-endian 16-bit words into a 32-bit
/// accumulator (handling an odd trailing byte), seeded with `init`.
fn sum16(data: &[u8], init: u32) -> u32 {
    let mut sum = init;
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        sum += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    if let [last] = chunks.remainder() {
        // Pad the final odd byte with a zero low byte.
        sum += u16::from_be_bytes([*last, 0]) as u32;
    }
    sum
}

/// Fold a 32-bit one's-complement sum down to 16 bits and complement it.
fn ones_complement_fold(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> UdpFrame<'static> {
        UdpFrame {
            dst_mac: MacAddr::new([0x52, 0x54, 0x00, 0x11, 0x22, 0x33]),
            src_mac: MacAddr::new([0x52, 0x54, 0x00, 0xaa, 0xbb, 0xcc]),
            src_ip: Ipv4Addr::new(10, 11, 0, 1),
            dst_ip: Ipv4Addr::new(10, 11, 0, 2),
            src_port: 40000,
            dst_port: 8001,
            ttl: 64,
            ip_id: 0x1234,
            payload: b"solana-shred-payload",
        }
    }

    #[test]
    fn ipv4_checksum_known_vector() {
        // Classic worked example from many networking texts. Header with the
        // checksum field zeroed; expected checksum is 0xb861.
        let hdr = [
            0x45u8, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8,
            0x00, 0x01, 0xc0, 0xa8, 0x00, 0xc7,
        ];
        assert_eq!(ipv4_checksum(&hdr), 0xb861);
    }

    #[test]
    fn ipv4_checksum_is_self_verifying() {
        // A correct IPv4 header (checksum field included) sums to 0 under the
        // one's-complement sum — receivers rely on this property.
        let frame = sample().encode();
        let ip = &frame[ETH_HDR_LEN..ETH_HDR_LEN + IPV4_HDR_LEN];
        let folded = ones_complement_fold(sum16(ip, 0));
        assert_eq!(folded, 0, "valid IPv4 header must checksum to zero");
    }

    #[test]
    fn roundtrip_build_then_parse() {
        let frame = sample().encode();
        let dg = parse_udp(&frame).expect("should parse our own frame");

        assert_eq!(dg.dst_mac, sample().dst_mac);
        assert_eq!(dg.src_mac, sample().src_mac);
        assert_eq!(dg.src_ip, Ipv4Addr::new(10, 11, 0, 1));
        assert_eq!(dg.dst_ip, Ipv4Addr::new(10, 11, 0, 2));
        assert_eq!(dg.src_port, 40000);
        assert_eq!(dg.dst_port, 8001);
        assert_eq!(dg.payload, b"solana-shred-payload");
    }

    #[test]
    fn encoded_len_matches_written() {
        let f = sample();
        let mut buf = vec![0u8; 2048];
        let n = f.encode_into(&mut buf);
        assert_eq!(n, f.encoded_len());
        assert_eq!(n, HDR_OVERHEAD + f.payload.len());
    }

    #[test]
    fn parse_rejects_non_ipv4() {
        let mut frame = sample().encode();
        // Corrupt the ethertype to ARP (0x0806).
        frame[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
        assert_eq!(parse_udp(&frame), Err(ParseError::NotIpv4(0x0806)));
    }

    #[test]
    fn parse_rejects_non_udp() {
        let mut frame = sample().encode();
        // Set IPv4 protocol to TCP (6).
        frame[ETH_HDR_LEN + 9] = 6;
        // Re-fix the IPv4 checksum so we fail on protocol, not length/csum.
        frame[ETH_HDR_LEN + 10..ETH_HDR_LEN + 12].copy_from_slice(&[0, 0]);
        let csum = ipv4_checksum(&frame[ETH_HDR_LEN..ETH_HDR_LEN + IPV4_HDR_LEN]);
        frame[ETH_HDR_LEN + 10..ETH_HDR_LEN + 12].copy_from_slice(&csum.to_be_bytes());
        assert_eq!(parse_udp(&frame), Err(ParseError::NotUdp(6)));
    }

    #[test]
    fn parse_rejects_truncated() {
        let frame = [0u8; 10];
        assert_eq!(
            parse_udp(&frame),
            Err(ParseError::TooShort {
                needed: ETH_HDR_LEN,
                got: 10
            })
        );
    }

    #[test]
    fn empty_payload_is_valid() {
        let mut f = sample();
        f.payload = b"";
        let frame = f.encode();
        let dg = parse_udp(&frame).unwrap();
        assert_eq!(dg.payload, b"");
    }

    #[test]
    fn udp_checksum_zero_becomes_all_ones() {
        // We don't easily force a true-zero checksum, but we can assert the
        // function never returns 0 for our sample (the reserved value).
        let frame = sample().encode();
        let udp_off = ETH_HDR_LEN + IPV4_HDR_LEN;
        let csum = u16::from_be_bytes([frame[udp_off + 6], frame[udp_off + 7]]);
        assert_ne!(csum, 0, "UDP checksum 0 must be transmitted as 0xffff");
    }
}
