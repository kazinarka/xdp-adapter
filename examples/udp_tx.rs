//! UDP transmitter — a skeleton "fast submit" path.
//!
//! Crafts and blasts UDP datagrams out an AF_XDP socket. In a real MEV service
//! this is the shape of a low-latency transaction-submit path that bypasses the
//! kernel UDP stack to shave syscall + stack-traversal latency when racing a
//! transaction to a leader's TPU.
//!
//! Because AF_XDP gives you raw L2 frames, you must supply the destination MAC
//! (the next hop / gateway). On the veth rig the peer's MAC is the right value.
//!
//! Run on Linux (needs root / CAP_NET_ADMIN):
//!     sudo ./target/release/examples/udp_tx <interface> <dst_mac> <dst_ip> <dst_port> [count]
//!
//! Example against the veth rig:
//!     DST_MAC=$(cat /sys/class/net/veth1/address)
//!     sudo ./target/release/examples/udp_tx veth0 $DST_MAC 10.11.0.2 8001 1000

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use rxdp::packet::{MacAddr, UdpFrame};
    use rxdp::{XdpConfig, XdpSocket};
    use std::net::Ipv4Addr;

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!("usage: udp_tx <interface> <dst_mac> <dst_ip> <dst_port> [count]");
        std::process::exit(2);
    }
    let if_name = &args[0];
    let dst_mac = parse_mac(&args[1]).expect("dst_mac like aa:bb:cc:dd:ee:ff");
    let dst_ip: Ipv4Addr = args[2].parse().expect("dst_ip");
    let dst_port: u16 = args[3].parse().expect("dst_port");
    let count: u64 = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(1);

    // Source addressing. In production, derive src_mac/src_ip from the chosen
    // interface; here we use the interface's own MAC if readable, else a dummy.
    let src_mac = read_if_mac(if_name).unwrap_or(MacAddr::new([0x02, 0, 0, 0, 0, 1]));
    let src_ip = Ipv4Addr::new(10, 11, 0, 1);

    let mut sock = XdpSocket::bind(XdpConfig::new(if_name.clone()))?;
    println!("sending {count} datagrams to {dst_ip}:{dst_port} via {if_name}");

    let payload = b"hello-from-af-xdp";
    let mut sent = 0u64;
    while sent < count {
        let frame = UdpFrame {
            dst_mac,
            src_mac,
            src_ip,
            dst_ip,
            src_port: 40000,
            dst_port,
            ttl: 64,
            ip_id: 0, // overwritten per-send by the socket's counter
            payload,
        };
        match sock.send(&frame) {
            Ok(()) => sent += 1,
            Err(rxdp::XdpError::NoFreeFrames) => {
                sock.reclaim_completions();
            }
            Err(e) => return Err(e.into()),
        }
    }
    sock.flush()?;
    println!("done: {sent} sent");
    Ok(())
}

#[cfg(target_os = "linux")]
fn parse_mac(s: &str) -> Option<rxdp::packet::MacAddr> {
    let mut o = [0u8; 6];
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    for (i, p) in parts.iter().enumerate() {
        o[i] = u8::from_str_radix(p, 16).ok()?;
    }
    Some(rxdp::packet::MacAddr::new(o))
}

#[cfg(target_os = "linux")]
fn read_if_mac(if_name: &str) -> Option<rxdp::packet::MacAddr> {
    let s = std::fs::read_to_string(format!("/sys/class/net/{if_name}/address")).ok()?;
    parse_mac(s.trim())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("udp_tx only runs on Linux (AF_XDP). Build and run it on your Linux box.");
}
