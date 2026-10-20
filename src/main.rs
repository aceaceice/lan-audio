mod audio;
mod discovery;
mod dsp;
mod protocol;

use clap::{Parser, Subcommand};
use discovery::{
    local_hostname, local_lan_ipv4, scan_lan_receivers, spawn_receiver_responder,
    spawn_sender_discovery_watcher, DiscoveredReceiver,
};
use dsp::{
    compute_peak_and_rms, decode_audio_packet, encode_audio_packet, format_vu_bar,
    AdpcmChannelState, JitterBuffer,
};
use protocol::{
    Codec, DEFAULT_AUDIO_PORT, DEFAULT_DISCOVERY_PORT, DEFAULT_PACKET_FRAMES,
};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(
    name = "lan-audio",
    version,
    about = "Ultra-low-latency LAN audio streamer between macOS and Linux devices"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Receive audio over LAN and play it on this computer's speakers/headphones
    Recv {
        /// Output audio device name or index (see `lan-audio devices`)
        #[arg(short, long)]
        device: Option<String>,

        /// UDP port to listen on for audio packets
        #[arg(short, long, default_value_t = DEFAULT_AUDIO_PORT)]
        port: u16,

        /// UDP port used for automatic LAN discovery
        #[arg(long, default_value_t = DEFAULT_DISCOVERY_PORT)]
        discovery_port: u16,

        /// Target jitter buffer latency in milliseconds (lower = less delay, higher = smoother over shaky Wi-Fi)
        #[arg(short, long, default_value_t = 25.0)]
        latency_ms: f32,

        /// Playback volume multiplier (1.0 = 100%, 1.5 = 150%)
        #[arg(short, long, default_value_t = 1.0)]
        volume: f32,

        /// Custom name announced on LAN (defaults to system hostname)
        #[arg(long)]
        name: Option<String>,
    },

    /// Capture audio on this computer and send it over LAN to receiver(s)
    Send {
        /// Target receiver IP or hostname (e.g. `192.168.1.50` or `192.168.1.50:54321`).
        /// If omitted, automatically discovers receivers on the LAN!
        #[arg(short, long)]
        target: Vec<String>,

        /// Input audio device name or index (see `lan-audio devices`).
        /// By default, automatically selects desktop loopback (BlackHole on macOS, PulseAudio monitor on Linux).
        #[arg(short, long)]
        device: Option<String>,

        /// Capture from default microphone instead of desktop loopback audio
        #[arg(long)]
        mic: bool,

        /// Audio codec (`pcm-i16` for uncompressed CD quality, `adpcm` for 50% lower Wi-Fi bandwidth, `pcm-f32` for float)
        #[arg(short, long, value_enum, default_value_t = Codec::PcmI16)]
        codec: Codec,

        /// Preferred capture sample rate in Hz
        #[arg(short, long, default_value_t = 48000)]
        rate: u32,

        /// Digital gain / volume multiplier applied before sending (1.0 = 100%)
        #[arg(short, long, default_value_t = 1.0)]
        volume: f32,

        /// UDP port used for automatic LAN discovery
        #[arg(long, default_value_t = DEFAULT_DISCOVERY_PORT)]
        discovery_port: u16,
    },

    /// Scan the local network for running `lan-audio recv` instances
    Discover {
        /// UDP discovery port
        #[arg(long, default_value_t = DEFAULT_DISCOVERY_PORT)]
        discovery_port: u16,

        /// Scan duration in milliseconds
        #[arg(short, long, default_value_t = 1500)]
        timeout_ms: u64,
    },

    /// List all available input, output, and loopback audio devices on this system
    Devices,
}

fn parse_target_addr(raw: &str) -> Result<SocketAddr, String> {
    let with_port = if raw.contains(':') {
        raw.to_string()
    } else {
        format!("{}:{}", raw, DEFAULT_AUDIO_PORT)
    };
    with_port
        .to_socket_addrs()
        .map_err(|e| format!("Invalid target '{}': {}", raw, e))?
        .find(|a| a.is_ipv4())
        .ok_or_else(|| format!("Could not resolve IPv4 address for '{}'", raw))
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Devices => {
            audio::list_devices();
        }
        Commands::Discover {
            discovery_port,
            timeout_ms,
        } => {
            println!(
                "Scanning LAN (port {}) for active receivers...",
                discovery_port
            );
            let found = scan_lan_receivers(discovery_port, Duration::from_millis(timeout_ms));
            if found.is_empty() {
                println!("No receivers found on LAN. Make sure `lan-audio recv` is running on your PC.");
            } else {
                println!("Found {} receiver(s):", found.len());
                for r in found {
                    println!(
                        "  * {} -> {}:{} ({} Hz, {}ch)",
                        r.hostname, r.ip, r.audio_port, r.sample_rate, r.channels
                    );
                }
            }
        }
        Commands::Recv {
            device,
            port,
            discovery_port,
            latency_ms,
            volume,
            name,
        } => {
            if let Err(e) = run_receiver(device, port, discovery_port, latency_ms, volume, name) {
                eprintln!("Error in receiver: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Send {
            target,
            device,
            mic,
            codec,
            rate,
            volume,
            discovery_port,
        } => {
            if let Err(e) =
                run_sender(target, device, mic, codec, rate, volume, discovery_port)
            {
                eprintln!("Error in sender: {}", e);
                std::process::exit(1);
            }
        }
    }
}

fn run_receiver(
    device_query: Option<String>,
    audio_port: u16,
    discovery_port: u16,
    latency_ms: f32,
    volume: f32,
    custom_name: Option<String>,
) -> Result<(), String> {
    let selected = audio::select_output_device(device_query.as_deref(), Some(48000))?;
    let output_rate = selected.config.sample_rate.0;
    let output_channels = selected.config.channels;
    let hostname = custom_name.unwrap_or_else(local_hostname);
    let local_ip = local_lan_ipv4()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "0.0.0.0".into());

    println!("============================================================");
    println!(" LAN Audio Receiver ({})", hostname);
    println!("============================================================");
    println!(
        "  Output Device : {} ({} Hz, {}ch, {:?})",
        selected.name, output_rate, output_channels, selected.sample_format
    );
    println!("  Listening On  : {}:{} (UDP)", local_ip, audio_port);
    println!("  Discovery     : UDP port {} (Auto-discovery active)", discovery_port);
    println!("  Target Latency: {:.1} ms", latency_ms);
    println!("============================================================");
    println!("Waiting for audio stream from your laptop (`lan-audio send`)...\n");

    let running = Arc::new(AtomicBool::new(true));
    let _responder = spawn_receiver_responder(
        discovery_port,
        hostname,
        audio_port,
        output_rate,
        2,
        Arc::clone(&running),
    )
    .map_err(|e| format!("Failed to bind discovery port {}: {}", discovery_port, e))?;

    let jitter = Arc::new(Mutex::new(JitterBuffer::new(output_rate, latency_ms)));
    let peak_bits = Arc::new(AtomicU32::new(0));

    let jitter_out = Arc::clone(&jitter);
    let peak_out = Arc::clone(&peak_bits);

    let _stream = audio::build_stereo_output_stream(&selected, move |out_stereo| {
        if let Ok(mut jb) = jitter_out.lock() {
            jb.pop_output_frames(out_stereo, volume);
        } else {
            out_stereo.fill([0.0, 0.0]);
        }
        let (peak, _) = compute_peak_and_rms(out_stereo);
        peak_out.store(peak.to_bits(), Ordering::Relaxed);
    })?;

    let sock = UdpSocket::bind(("0.0.0.0", audio_port))
        .map_err(|e| format!("Failed to bind UDP audio port {}: {}", audio_port, e))?;
    sock.set_read_timeout(Some(Duration::from_millis(250)))
        .map_err(|e| e.to_string())?;

    let mut packet_buf = [0u8; 4096];
    let mut decoded_frames = Vec::with_capacity(1024);
    let mut last_status = Instant::now();
    let mut last_sender: Option<SocketAddr> = None;
    let mut last_codec: Option<Codec> = None;

    loop {
        match sock.recv_from(&mut packet_buf) {
            Ok((len, src)) => {
                if let Some(hdr) = decode_audio_packet(&packet_buf[..len], &mut decoded_frames) {
                    if last_sender != Some(src) {
                        println!("\n[+] Connected stream from {} ({}, {} Hz)", src, hdr.codec, hdr.sample_rate);
                        last_sender = Some(src);
                    }
                    last_codec = Some(hdr.codec);
                    if let Ok(mut jb) = jitter.lock() {
                        jb.push_packet(&hdr, &decoded_frames);
                    }
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                eprintln!("\n[!] UDP receive error: {}", e);
            }
        }

        if last_status.elapsed() >= Duration::from_millis(200) {
            last_status = Instant::now();
            if let Some(sender) = last_sender {
                if let Ok(jb) = jitter.lock() {
                    let peak = f32::from_bits(peak_bits.load(Ordering::Relaxed));
                    let vu = format_vu_bar(peak, 20);
                    let codec_str = last_codec.map(|c| c.to_string()).unwrap_or_else(|| "-".into());
                    print!(
                        "\r  Stream: {} | {} {}Hz -> {}Hz | Buf: {:4.1}ms | Pkts: {} (Lost: {}, Underrun: {}) {}   ",
                        sender.ip(),
                        codec_str,
                        jb.sender_sample_rate(),
                        output_rate,
                        jb.current_latency_ms(),
                        jb.packets_received,
                        jb.packets_lost,
                        jb.underruns,
                        vu
                    );
                    let _ = std::io::stdout().flush();
                }
            }
        }
    }
}

fn run_sender(
    raw_targets: Vec<String>,
    device_query: Option<String>,
    use_mic: bool,
    codec: Codec,
    preferred_rate: u32,
    volume: f32,
    discovery_port: u16,
) -> Result<(), String> {
    let running = Arc::new(AtomicBool::new(true));
    let receivers: Arc<Mutex<HashMap<SocketAddr, DiscoveredReceiver>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let mut static_targets = Vec::new();
    for raw in &raw_targets {
        let addr = parse_target_addr(raw)?;
        static_targets.push(addr);
    }

    if static_targets.is_empty() {
        println!(
            "Auto-discovery enabled (UDP port {}). Scanning for LAN receivers...",
            discovery_port
        );
        let initial = scan_lan_receivers(discovery_port, Duration::from_millis(900));
        if let Ok(mut map) = receivers.lock() {
            for r in initial {
                println!(
                    "[+] Found receiver: {} ({}:{})",
                    r.hostname, r.ip, r.audio_port
                );
                map.insert(r.audio_socket_addr(), r);
            }
        }
        let _watcher = spawn_sender_discovery_watcher(
            discovery_port,
            Arc::clone(&receivers),
            Arc::clone(&running),
        )
        .map_err(|e| format!("Failed to start discovery watcher: {}", e))?;
    } else {
        for addr in &static_targets {
            println!("[+] Target receiver: {}", addr);
        }
    }

    let sock = UdpSocket::bind("0.0.0.0:0")
        .map_err(|e| format!("Failed to bind sender UDP socket: {}", e))?;

    // On Linux, if neither `--mic` nor `--device` was specified, prefer PulseAudio/PipeWire desktop monitor if available
    #[cfg(target_os = "linux")]
    if !use_mic && device_query.is_none() {
        if let Some(monitor_source) = audio::linux_default_pulse_monitor() {
            if Command::new("parecord").arg("--version").output().is_ok() {
                return run_linux_pulse_monitor_sender(
                    &monitor_source,
                    preferred_rate,
                    codec,
                    volume,
                    sock,
                    static_targets,
                    receivers,
                );
            }
        }
    }

    let selected = audio::select_input_device(
        device_query.as_deref(),
        !use_mic,
        Some(preferred_rate),
    )?;
    let sample_rate = selected.config.sample_rate.0;

    println!("============================================================");
    println!(" LAN Audio Sender ({})", local_hostname());
    println!("============================================================");
    println!(
        "  Input Device : {} ({} Hz, {}ch, {:?})",
        selected.name, sample_rate, selected.config.channels, selected.sample_format
    );
    println!("  Codec        : {}", codec);
    println!("============================================================");

    let packets_sent = Arc::new(AtomicU64::new(0));
    let bytes_sent = Arc::new(AtomicU64::new(0));
    let peak_bits = Arc::new(AtomicU32::new(0));

    let packets_cb = Arc::clone(&packets_sent);
    let bytes_cb = Arc::clone(&bytes_sent);
    let peak_cb = Arc::clone(&peak_bits);
    let receivers_cb = Arc::clone(&receivers);
    let static_targets_cb = static_targets.clone();

    let mut frame_accum: Vec<[f32; 2]> = Vec::with_capacity(DEFAULT_PACKET_FRAMES * 2);
    let mut packet_buf: Vec<u8> = Vec::with_capacity(1500);
    let mut adpcm_states = [AdpcmChannelState::default(), AdpcmChannelState::default()];
    let mut seq: u32 = 0;

    let _stream = audio::build_stereo_input_stream(&selected, move |stereo_chunk| {
        let (peak, _) = compute_peak_and_rms(stereo_chunk);
        peak_cb.store((peak * volume).to_bits(), Ordering::Relaxed);

        for &[l, r] in stereo_chunk {
            frame_accum.push([l * volume, r * volume]);
            if frame_accum.len() >= DEFAULT_PACKET_FRAMES {
                encode_audio_packet(
                    codec,
                    seq,
                    sample_rate,
                    &frame_accum[..DEFAULT_PACKET_FRAMES],
                    &mut adpcm_states,
                    &mut packet_buf,
                );
                seq = seq.wrapping_add(1);
                frame_accum.clear();

                send_packet_to_targets(
                    &sock,
                    &packet_buf,
                    &static_targets_cb,
                    &receivers_cb,
                    &packets_cb,
                    &bytes_cb,
                );
            }
        }
    })?;

    print_sender_status_loop(codec, sample_rate, &packets_sent, &bytes_sent, &peak_bits, &static_targets, &receivers)
}

#[cfg(target_os = "linux")]
fn run_linux_pulse_monitor_sender(
    monitor_source: &str,
    sample_rate: u32,
    codec: Codec,
    volume: f32,
    sock: UdpSocket,
    static_targets: Vec<SocketAddr>,
    receivers: Arc<Mutex<HashMap<SocketAddr, DiscoveredReceiver>>>,
) -> Result<(), String> {
    println!("============================================================");
    println!(" LAN Audio Sender ({})", local_hostname());
    println!("============================================================");
    println!(
        "  Input Source : {} (Linux Desktop Audio Monitor, {} Hz, 2ch)",
        monitor_source, sample_rate
    );
    println!("  Codec        : {}", codec);
    println!("============================================================");

    let mut child = Command::new("parecord")
        .args([
            "--raw",
            "--format=float32le",
            &format!("--rate={}", sample_rate),
            "--channels=2",
            "--latency-msec=5",
            "-d",
            monitor_source,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn parecord for desktop audio capture: {}", e))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to open parecord stdout".to_string())?;

    let packets_sent = Arc::new(AtomicU64::new(0));
    let bytes_sent = Arc::new(AtomicU64::new(0));
    let peak_bits = Arc::new(AtomicU32::new(0));

    let packets_cb = Arc::clone(&packets_sent);
    let bytes_cb = Arc::clone(&bytes_sent);
    let peak_cb = Arc::clone(&peak_bits);
    let receivers_cb = Arc::clone(&receivers);
    let static_targets_cb = static_targets.clone();

    thread::spawn(move || {
        let mut raw_bytes = vec![0u8; DEFAULT_PACKET_FRAMES * 8];
        let mut frames = vec![[0.0f32; 2]; DEFAULT_PACKET_FRAMES];
        let mut packet_buf = Vec::with_capacity(1500);
        let mut adpcm_states = [AdpcmChannelState::default(), AdpcmChannelState::default()];
        let mut seq: u32 = 0;

        while stdout.read_exact(&mut raw_bytes).is_ok() {
            for i in 0..DEFAULT_PACKET_FRAMES {
                let b = i * 8;
                let l = f32::from_le_bytes([
                    raw_bytes[b],
                    raw_bytes[b + 1],
                    raw_bytes[b + 2],
                    raw_bytes[b + 3],
                ]) * volume;
                let r = f32::from_le_bytes([
                    raw_bytes[b + 4],
                    raw_bytes[b + 5],
                    raw_bytes[b + 6],
                    raw_bytes[b + 7],
                ]) * volume;
                frames[i] = [l, r];
            }
            let (peak, _) = compute_peak_and_rms(&frames);
            peak_cb.store(peak.to_bits(), Ordering::Relaxed);

            encode_audio_packet(
                codec,
                seq,
                sample_rate,
                &frames,
                &mut adpcm_states,
                &mut packet_buf,
            );
            seq = seq.wrapping_add(1);

            send_packet_to_targets(
                &sock,
                &packet_buf,
                &static_targets_cb,
                &receivers_cb,
                &packets_cb,
                &bytes_cb,
            );
        }
    });

    print_sender_status_loop(
        codec,
        sample_rate,
        &packets_sent,
        &bytes_sent,
        &peak_bits,
        &static_targets,
        &receivers,
    )
}

fn send_packet_to_targets(
    sock: &UdpSocket,
    packet_buf: &[u8],
    static_targets: &[SocketAddr],
    receivers: &Arc<Mutex<HashMap<SocketAddr, DiscoveredReceiver>>>,
    packets_sent: &AtomicU64,
    bytes_sent: &AtomicU64,
) {
    if !static_targets.is_empty() {
        for addr in static_targets {
            if sock.send_to(packet_buf, addr).is_ok() {
                packets_sent.fetch_add(1, Ordering::Relaxed);
                bytes_sent.fetch_add(packet_buf.len() as u64, Ordering::Relaxed);
            }
        }
    } else if let Ok(map) = receivers.lock() {
        for addr in map.keys() {
            if sock.send_to(packet_buf, addr).is_ok() {
                packets_sent.fetch_add(1, Ordering::Relaxed);
                bytes_sent.fetch_add(packet_buf.len() as u64, Ordering::Relaxed);
            }
        }
    }
}

fn print_sender_status_loop(
    codec: Codec,
    sample_rate: u32,
    packets_sent: &AtomicU64,
    bytes_sent: &AtomicU64,
    peak_bits: &AtomicU32,
    static_targets: &[SocketAddr],
    receivers: &Arc<Mutex<HashMap<SocketAddr, DiscoveredReceiver>>>,
) -> Result<(), String> {
    let mut prev_bytes = 0u64;
    let mut last_tick = Instant::now();

    loop {
        thread::sleep(Duration::from_millis(250));
        let dt = last_tick.elapsed().as_secs_f64().max(0.001);
        last_tick = Instant::now();

        let total_bytes = bytes_sent.load(Ordering::Relaxed);
        let kbps = ((total_bytes.saturating_sub(prev_bytes) as f64) * 8.0 / 1000.0) / dt;
        prev_bytes = total_bytes;

        let pkts = packets_sent.load(Ordering::Relaxed);
        let peak = f32::from_bits(peak_bits.load(Ordering::Relaxed));
        let vu = format_vu_bar(peak, 20);

        let target_count = if !static_targets.is_empty() {
            static_targets.len()
        } else {
            receivers.lock().map(|m| m.len()).unwrap_or(0)
        };

        print!(
            "\r  Receivers: {} | {} {}Hz | {:6.1} kbps | Pkts: {} {}   ",
            target_count, codec, sample_rate, kbps, pkts, vu
        );
        let _ = std::io::stdout().flush();
    }
}
