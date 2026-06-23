//! Zero-copy UDP receiver — a skeleton "shred listener".
//!
//! Binds an AF_XDP socket to a NIC queue and prints a line per received UDP
//! datagram. In a real Solana MEV service the `on_udp` closure is where you'd
//! hand the payload to a shred deshredder / transaction decoder.
//!
//! Run on Linux (needs root / CAP_NET_ADMIN):
//!     sudo ./target/release/examples/udp_rx <interface> [queue_id]
//!
//! Try it against the veth rig (see Makefile `make veth`):
//!     sudo ./target/release/examples/udp_rx veth1

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use rxdp::{XdpConfig, XdpSocket};

    let mut args = std::env::args().skip(1);
    let if_name = args.next().unwrap_or_else(|| {
        eprintln!("usage: udp_rx <interface> [queue_id]");
        std::process::exit(2);
    });
    let queue_id: u32 = args.next().map(|s| s.parse().unwrap()).unwrap_or(0);

    let cfg = XdpConfig {
        queue_id,
        ..XdpConfig::new(if_name)
    };
    println!(
        "binding AF_XDP socket on {}:{} ...",
        cfg.if_name, cfg.queue_id
    );
    let mut sock = XdpSocket::bind(cfg)?;
    println!("listening. Ctrl-C to stop.");

    let mut total: u64 = 0;
    loop {
        sock.recv_udp(|dg| {
            total += 1;
            println!(
                "#{total} {}:{} -> {}:{}  {} bytes  payload[0..16]={:02x?}",
                dg.src_ip,
                dg.src_port,
                dg.dst_ip,
                dg.dst_port,
                dg.payload.len(),
                &dg.payload[..dg.payload.len().min(16)],
            );
        })?;
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("udp_rx only runs on Linux (AF_XDP). Build and run it on your Linux box.");
}
