//! Integration test: send through one AF_XDP socket, receive on its veth peer.
//!
//! This is the real end-to-end exercise of the transport. It is `#[ignore]`d by
//! default because it requires:
//!   - Linux,
//!   - root / CAP_NET_ADMIN,
//!   - a `veth` pair set up as `veth0 <-> veth1` (run `make veth` first).
//!
//! Run it with:
//!     make veth
//!     sudo -E cargo test --test loopback -- --ignored --nocapture
//!     make veth-down
//!
//! Why veth? It gives us a deterministic, hardware-free L2 link. Packets sent
//! on `veth0` arrive on `veth1`. AF_XDP runs in generic/SKB mode on veth, which
//! needs no driver support — perfect for CI and local correctness testing.

#![cfg(target_os = "linux")]

use rxdp::packet::{MacAddr, UdpFrame};
use rxdp::{XdpConfig, XdpSocket};
use std::net::Ipv4Addr;

fn if_mac(name: &str) -> MacAddr {
    let s = std::fs::read_to_string(format!("/sys/class/net/{name}/address"))
        .unwrap_or_else(|_| panic!("interface {name} not found — run `make veth`"));
    let mut o = [0u8; 6];
    for (i, p) in s.trim().split(':').enumerate() {
        o[i] = u8::from_str_radix(p, 16).unwrap();
    }
    MacAddr::new(o)
}

#[test]
#[ignore = "requires root + veth pair; run via `make test-integration`"]
fn veth_udp_roundtrip() {
    // Receiver on veth1, sender on veth0.
    let mut rx = XdpSocket::bind(XdpConfig::new("veth1")).expect("bind veth1");
    let mut tx = XdpSocket::bind(XdpConfig::new("veth0")).expect("bind veth0");

    let payload = b"xdp-loopback-probe-0xC0FFEE";
    let frame = UdpFrame {
        dst_mac: if_mac("veth1"),
        src_mac: if_mac("veth0"),
        src_ip: Ipv4Addr::new(10, 11, 0, 1),
        dst_ip: Ipv4Addr::new(10, 11, 0, 2),
        src_port: 40000,
        dst_port: 8001,
        ttl: 64,
        ip_id: 0,
        payload,
    };

    tx.send(&frame).expect("send");
    tx.flush().expect("flush");

    // Poll the receiver for a short while; SKB-mode delivery is not instant.
    let mut got: Option<Vec<u8>> = None;
    for _ in 0..50 {
        rx.recv_udp(|dg| {
            if dg.dst_port == 8001 {
                got = Some(dg.payload.to_vec());
            }
        })
        .expect("recv");
        if got.is_some() {
            break;
        }
    }

    let received = got.expect("did not receive the probe datagram within timeout");
    assert_eq!(received, payload, "payload survived the round trip intact");
}
