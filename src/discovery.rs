use crate::protocol::{DiscoveryMessage, MULTICAST_ADDR};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct DiscoveredReceiver {
    pub hostname: String,
    pub ip: Ipv4Addr,
    pub audio_port: u16,
    pub sample_rate: u32,
    pub channels: u8,
    pub last_seen: Instant,
}

impl DiscoveredReceiver {
    pub fn audio_socket_addr(&self) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(self.ip, self.audio_port))
    }
}

pub fn local_hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME") {
        let trimmed = h.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(content) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(out) = Command::new("hostname").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "lan-audio-node".to_string()
}

/// Detect the primary local LAN IPv4 address without sending any network packets
pub fn local_lan_ipv4() -> Option<Ipv4Addr> {
    for probe in ["192.168.1.1:80", "10.0.0.1:80", "1.1.1.1:80"] {
        if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
            if sock.connect(probe).is_ok() {
                if let Ok(SocketAddr::V4(local)) = sock.local_addr() {
                    let ip = *local.ip();
                    if !ip.is_loopback() && !ip.is_unspecified() {
                        return Some(ip);
                    }
                }
            }
        }
    }
    None
}

fn broadcast_targets(discovery_port: u16) -> Vec<SocketAddrV4> {
    let mut targets = vec![
        SocketAddrV4::new(Ipv4Addr::from(MULTICAST_ADDR), discovery_port),
        SocketAddrV4::new(Ipv4Addr::BROADCAST, discovery_port),
    ];
    if let Some(ip) = local_lan_ipv4() {
        let octets = ip.octets();
        let subnet_bcast = Ipv4Addr::new(octets[0], octets[1], octets[2], 255);
        let addr = SocketAddrV4::new(subnet_bcast, discovery_port);
        if !targets.contains(&addr) {
            targets.push(addr);
        }
    }
    targets
}

/// Spawn a responder thread for `lan-audio recv` so senders on the LAN can find this receiver
pub fn spawn_receiver_responder(
    discovery_port: u16,
    hostname: String,
    audio_port: u16,
    sample_rate: u32,
    channels: u8,
    running: Arc<AtomicBool>,
) -> std::io::Result<thread::JoinHandle<()>> {
    let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, discovery_port))?;
    let _ = sock.set_broadcast(true);
    let _ = sock.join_multicast_v4(&Ipv4Addr::from(MULTICAST_ADDR), &Ipv4Addr::UNSPECIFIED);
    sock.set_read_timeout(Some(Duration::from_millis(250)))?;

    let handle = thread::Builder::new()
        .name("lan-discovery-responder".into())
        .spawn(move || {
            let announce = DiscoveryMessage::ReceiverAnnounce {
                hostname,
                audio_port,
                sample_rate,
                channels,
            }
            .encode();

            let targets = broadcast_targets(discovery_port);
            let mut last_broadcast = Instant::now() - Duration::from_secs(10);
            let mut buf = [0u8; 256];

            while running.load(Ordering::Relaxed) {
                if last_broadcast.elapsed() >= Duration::from_millis(1500) {
                    for target in &targets {
                        let _ = sock.send_to(&announce, target);
                    }
                    last_broadcast = Instant::now();
                }

                if let Ok((len, src)) = sock.recv_from(&mut buf) {
                    if let Some(DiscoveryMessage::Query { .. }) =
                        DiscoveryMessage::decode(&buf[..len])
                    {
                        // Immediately reply via unicast to the querying sender
                        let _ = sock.send_to(&announce, src);
                    }
                }
            }
        })?;

    Ok(handle)
}

/// Perform a one-shot discovery scan on the LAN for `duration`
pub fn scan_lan_receivers(discovery_port: u16, duration: Duration) -> Vec<DiscoveredReceiver> {
    let mut found: HashMap<SocketAddrV4, DiscoveredReceiver> = HashMap::new();
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
        return Vec::new();
    };
    let _ = sock.set_broadcast(true);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));

    let query = DiscoveryMessage::Query {
        sender_name: local_hostname(),
    }
    .encode();
    let targets = broadcast_targets(discovery_port);

    let start = Instant::now();
    let mut last_query = Instant::now() - Duration::from_secs(5);
    let mut buf = [0u8; 256];

    while start.elapsed() < duration {
        if last_query.elapsed() >= Duration::from_millis(300) {
            for t in &targets {
                let _ = sock.send_to(&query, t);
            }
            last_query = Instant::now();
        }

        if let Ok((len, SocketAddr::V4(src))) = sock.recv_from(&mut buf) {
            if let Some(DiscoveryMessage::ReceiverAnnounce {
                hostname,
                audio_port,
                sample_rate,
                channels,
            }) = DiscoveryMessage::decode(&buf[..len])
            {
                let key = SocketAddrV4::new(*src.ip(), audio_port);
                found.insert(
                    key,
                    DiscoveredReceiver {
                        hostname,
                        ip: *src.ip(),
                        audio_port,
                        sample_rate,
                        channels,
                        last_seen: Instant::now(),
                    },
                );
            }
        }
    }

    let mut list: Vec<_> = found.into_values().collect();
    list.sort_by_key(|r| (r.ip, r.audio_port));
    list
}

/// Spawn a continuous discovery watcher for `lan-audio send` so receivers can join/leave dynamically
pub fn spawn_sender_discovery_watcher(
    discovery_port: u16,
    receivers: Arc<Mutex<HashMap<SocketAddr, DiscoveredReceiver>>>,
    running: Arc<AtomicBool>,
) -> std::io::Result<thread::JoinHandle<()>> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    let _ = sock.set_broadcast(true);
    sock.set_read_timeout(Some(Duration::from_millis(250)))?;

    let handle = thread::Builder::new()
        .name("lan-discovery-watcher".into())
        .spawn(move || {
            let query = DiscoveryMessage::Query {
                sender_name: local_hostname(),
            }
            .encode();
            let targets = broadcast_targets(discovery_port);
            let mut last_query = Instant::now() - Duration::from_secs(10);
            let mut buf = [0u8; 256];

            while running.load(Ordering::Relaxed) {
                if last_query.elapsed() >= Duration::from_secs(2) {
                    for t in &targets {
                        let _ = sock.send_to(&query, t);
                    }
                    last_query = Instant::now();

                    // Prune receivers not seen in 10 seconds
                    if let Ok(mut map) = receivers.lock() {
                        map.retain(|_, r| r.last_seen.elapsed() < Duration::from_secs(10));
                    }
                }

                if let Ok((len, SocketAddr::V4(src))) = sock.recv_from(&mut buf) {
                    if let Some(DiscoveryMessage::ReceiverAnnounce {
                        hostname,
                        audio_port,
                        sample_rate,
                        channels,
                    }) = DiscoveryMessage::decode(&buf[..len])
                    {
                        let audio_addr = SocketAddr::V4(SocketAddrV4::new(*src.ip(), audio_port));
                        if let Ok(mut map) = receivers.lock() {
                            let is_new = !map.contains_key(&audio_addr);
                            map.insert(
                                audio_addr,
                                DiscoveredReceiver {
                                    hostname: hostname.clone(),
                                    ip: *src.ip(),
                                    audio_port,
                                    sample_rate,
                                    channels,
                                    last_seen: Instant::now(),
                                },
                            );
                            if is_new {
                                eprintln!(
                                    "\n[+] Auto-discovered receiver: {} ({}) [{} Hz, {}ch]",
                                    hostname, audio_addr, sample_rate, channels
                                );
                            }
                        }
                    }
                }
            }
        })?;

    Ok(handle)
}
