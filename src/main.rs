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
    Codec, DEFAULT_AUDIO_PORT, DEFAULT_DISCOVERY_PORT, DEFAULT_PACKET_FRAMES, FLAG_STREAM_RESUMED,
};
use std::collections::HashMap;
use std::io::Write;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
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
        #[arg(short, long, default_value_t = 20.0)]
        latency_ms: f32,

        /// Maximum jitter buffer cap in milliseconds before trimming bursts (defaults to ~2.25x target latency)
        #[arg(long)]
        max_latency_ms: Option<f32>,

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

        /// Disable automatic silence pause (DTX) and stream silent packets continuously even when no audio is playing
        #[arg(long)]
        no_silence_pause: bool,

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
                println!(
                    "No receivers found on LAN. Make sure `lan-audio recv` is running on your PC."
                );
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
            max_latency_ms,
            volume,
            name,
        } => {
            if let Err(e) = run_receiver(
                device,
                port,
                discovery_port,
                latency_ms,
                max_latency_ms,
                volume,
                name,
            ) {
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
            no_silence_pause,
            discovery_port,
        } => {
            if let Err(e) = run_sender(
                target,
                device,
                mic,
                codec,
                rate,
                volume,
                !no_silence_pause,
                discovery_port,
            ) {
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
    max_latency_ms: Option<f32>,
    volume: f32,
    custom_name: Option<String>,
) -> Result<(), String> {
    let hostname = custom_name.unwrap_or_else(local_hostname);
    let local_ip = local_lan_ipv4()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "0.0.0.0".into());

    let running = Arc::new(AtomicBool::new(true));
    let output_rate: u32 = 48000;
    let jitter = Arc::new(Mutex::new(JitterBuffer::new(
        output_rate,
        latency_ms,
        max_latency_ms,
    )));
    let peak_bits = Arc::new(AtomicU32::new(0));

    // On Linux, use direct low-latency libpulse-simple playback (10ms buffer, 2.5ms chunks)
    // unless a specific non-pulse ALSA device was requested.
    let mut _cpal_stream: Option<cpal::Stream> = None;
    let mut _pulse_thread: Option<thread::JoinHandle<()>> = None;
    let mut backend_desc = String::new();
    let mut actual_output_rate = output_rate;

    #[cfg(target_os = "linux")]
    if device_query.is_none() || device_query.as_deref() == Some("pulse") {
        let jitter_out = Arc::clone(&jitter);
        let peak_out = Arc::clone(&peak_bits);
        if let Ok(handle) = audio::pulse_ffi::spawn_low_latency_playback(
            None,
            output_rate,
            latency_ms,
            Arc::clone(&running),
            move |out_stereo| {
                if let Ok(mut jb) = jitter_out.lock() {
                    jb.pop_output_frames(out_stereo, volume);
                } else {
                    out_stereo.fill([0.0, 0.0]);
                }
                let (peak, _) = compute_peak_and_rms(out_stereo);
                peak_out.store(peak.to_bits(), Ordering::Relaxed);
            },
        ) {
            _pulse_thread = Some(handle);
            backend_desc = format!("PulseAudio Low-Latency Direct ({} Hz, 2ch, F32)", output_rate);
        }
    }

    if _pulse_thread.is_none() {
        let selected = audio::select_output_device(device_query.as_deref(), Some(48000))?;
        actual_output_rate = selected.config.sample_rate.0;
        if let Ok(mut jb) = jitter.lock() {
            *jb = JitterBuffer::new(actual_output_rate, latency_ms, max_latency_ms);
        }
        backend_desc = format!(
            "{} ({} Hz, {}ch, {:?})",
            selected.name, actual_output_rate, selected.config.channels, selected.sample_format
        );
        let jitter_out = Arc::clone(&jitter);
        let peak_out = Arc::clone(&peak_bits);
        let stream = audio::build_stereo_output_stream(&selected, move |out_stereo| {
            if let Ok(mut jb) = jitter_out.lock() {
                jb.pop_output_frames(out_stereo, volume);
            } else {
                out_stereo.fill([0.0, 0.0]);
            }
            let (peak, _) = compute_peak_and_rms(out_stereo);
            peak_out.store(peak.to_bits(), Ordering::Relaxed);
        })?;
        _cpal_stream = Some(stream);
    }

    let effective_max_ms = jitter
        .lock()
        .map(|jb| jb.max_latency_ms())
        .unwrap_or(latency_ms * 2.25);

    println!("============================================================");
    println!(" LAN Audio Receiver ({})", hostname);
    println!("============================================================");
    println!("  Output Device : {}", backend_desc);
    println!("  Listening On  : {}:{} (UDP)", local_ip, audio_port);
    println!(
        "  Discovery     : UDP port {} (Auto-discovery active)",
        discovery_port
    );
    println!(
        "  Target Latency: {:.1} ms (Max Cap: {:.1} ms)",
        latency_ms, effective_max_ms
    );
    println!("============================================================");
    println!("Waiting for audio stream from your laptop (`lan-audio send`)...\n");

    let _responder = spawn_receiver_responder(
        discovery_port,
        hostname,
        audio_port,
        actual_output_rate,
        2,
        Arc::clone(&running),
    )
    .map_err(|e| format!("Failed to bind discovery port {}: {}", discovery_port, e))?;

    let sock = UdpSocket::bind(("0.0.0.0", audio_port))
        .map_err(|e| format!("Failed to bind UDP audio port {}: {}", audio_port, e))?;

    let last_sender: Arc<Mutex<Option<(SocketAddr, Codec)>>> = Arc::new(Mutex::new(None));
    let last_sender_net = Arc::clone(&last_sender);
    let jitter_net = Arc::clone(&jitter);

    // Dedicated high-priority network receive thread (never blocked by terminal stdout)
    thread::Builder::new()
        .name("udp-audio-recv".into())
        .spawn(move || {
            let mut packet_buf = [0u8; 4096];
            let mut decoded_frames = Vec::with_capacity(512);
            let mut current_src: Option<SocketAddr> = None;

            loop {
                if let Ok((len, src)) = sock.recv_from(&mut packet_buf) {
                    if let Some(hdr) = decode_audio_packet(&packet_buf[..len], &mut decoded_frames)
                    {
                        if current_src != Some(src) {
                            current_src = Some(src);
                            println!(
                                "\n[+] Connected stream from {} ({}, {} Hz)",
                                src, hdr.codec, hdr.sample_rate
                            );
                            if let Ok(mut ls) = last_sender_net.lock() {
                                *ls = Some((src, hdr.codec));
                            }
                        }
                        if let Ok(mut jb) = jitter_net.lock() {
                            jb.push_packet(&hdr, &decoded_frames);
                        }
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;

    // Status display loop on main thread
    loop {
        thread::sleep(Duration::from_millis(200));
        let sender_info = last_sender.lock().ok().and_then(|g| *g);
        if let Some((sender, codec)) = sender_info {
            if let Ok(jb) = jitter.lock() {
                let peak = f32::from_bits(peak_bits.load(Ordering::Relaxed));
                let vu = format_vu_bar(peak, 20);
                let buf_str = if jb.is_active() {
                    format!("{:4.1}ms", jb.current_latency_ms())
                } else {
                    "Idle  ".to_string()
                };
                print!(
                    "\r  Stream: {} | {} {}Hz -> {}Hz | Buf: {} | Pkts: {} (Lost: {}, Underrun: {}) {}   ",
                    sender.ip(),
                    codec,
                    jb.sender_sample_rate(),
                    actual_output_rate,
                    buf_str,
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

fn run_sender(
    raw_targets: Vec<String>,
    device_query: Option<String>,
    use_mic: bool,
    codec: Codec,
    preferred_rate: u32,
    volume: f32,
    silence_pause: bool,
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

    let packets_sent = Arc::new(AtomicU64::new(0));
    let bytes_sent = Arc::new(AtomicU64::new(0));
    let peak_bits = Arc::new(AtomicU32::new(0));
    let stream_paused = Arc::new(AtomicBool::new(false));

    // Bounded real-time channel so the CoreAudio/PulseAudio callback never waits on network syscalls or mutexes
    let (audio_tx, audio_rx) = mpsc::sync_channel::<Vec<[f32; 2]>>(64);

    let mut _cpal_stream: Option<cpal::Stream> = None;
    let mut _pulse_thread: Option<thread::JoinHandle<()>> = None;
    let mut sample_rate = preferred_rate;
    let mut input_desc = String::new();

    #[cfg(target_os = "linux")]
    if !use_mic && device_query.is_none() {
        if let Some(monitor_source) = audio::linux_default_pulse_monitor() {
            let tx_pulse = audio_tx.clone();
            if let Ok(handle) = audio::pulse_ffi::spawn_low_latency_capture(
                &monitor_source,
                preferred_rate,
                DEFAULT_PACKET_FRAMES,
                Arc::clone(&running),
                move |frames| {
                    let _ = tx_pulse.try_send(frames.to_vec());
                },
            ) {
                _pulse_thread = Some(handle);
                input_desc = format!(
                    "{} (Linux Desktop Audio Monitor, {} Hz, 2ch)",
                    monitor_source, preferred_rate
                );
            }
        }
    }

    if _pulse_thread.is_none() {
        let selected = audio::select_input_device(
            device_query.as_deref(),
            !use_mic,
            Some(preferred_rate),
        )?;
        sample_rate = selected.config.sample_rate.0;
        input_desc = format!(
            "{} ({} Hz, {}ch, {:?})",
            selected.name, sample_rate, selected.config.channels, selected.sample_format
        );
        let tx_cpal = audio_tx;
        let stream = audio::build_stereo_input_stream(&selected, move |stereo_chunk| {
            let _ = tx_cpal.try_send(stereo_chunk.to_vec());
        })?;
        _cpal_stream = Some(stream);
    }

    println!("============================================================");
    println!(" LAN Audio Sender ({})", local_hostname());
    println!("============================================================");
    println!("  Input Device : {}", input_desc);
    println!(
        "  Codec        : {} ({} frames = {:.1}ms/pkt)",
        codec,
        DEFAULT_PACKET_FRAMES,
        (DEFAULT_PACKET_FRAMES as f32 * 1000.0) / sample_rate as f32
    );
    println!(
        "  Silence Pause: {}",
        if silence_pause {
            "Enabled (pauses UDP after 500ms of silence, 0ms instant resume)"
        } else {
            "Disabled (streaming continuously)"
        }
    );
    println!("============================================================");

    let packets_cb = Arc::clone(&packets_sent);
    let bytes_cb = Arc::clone(&bytes_sent);
    let peak_cb = Arc::clone(&peak_bits);
    let paused_cb = Arc::clone(&stream_paused);
    let receivers_cb = Arc::clone(&receivers);
    let static_targets_cb = static_targets.clone();

    // Dedicated network packetizer & UDP transmitter thread
    thread::Builder::new()
        .name("udp-audio-send".into())
        .spawn(move || {
            let mut frame_accum: Vec<[f32; 2]> = Vec::with_capacity(DEFAULT_PACKET_FRAMES * 4);
            let mut packet_buf: Vec<u8> = Vec::with_capacity(1500);
            let mut adpcm_states = [AdpcmChannelState::default(), AdpcmChannelState::default()];
            let mut seq: u32 = 0;

            // Silence suppression (DTX): ~ -82 dBFS threshold, 500ms hangover tail
            const SILENCE_THRESHOLD: f32 = 0.00008;
            let hangover_packets = ((sample_rate as usize / DEFAULT_PACKET_FRAMES) / 2).max(40);
            let mut silent_packets_run: usize = 0;
            let mut is_paused = false;

            while let Ok(chunk) = audio_rx.recv() {
                let (peak, _) = compute_peak_and_rms(&chunk);
                peak_cb.store((peak * volume).to_bits(), Ordering::Relaxed);

                for &[l, r] in &chunk {
                    frame_accum.push([l * volume, r * volume]);
                    if frame_accum.len() >= DEFAULT_PACKET_FRAMES {
                        let pkt_slice = &mut frame_accum[..DEFAULT_PACKET_FRAMES];
                        let (pkt_peak, _) = compute_peak_and_rms(pkt_slice);

                        let mut flags = 0u8;
                        if silence_pause {
                            if pkt_peak <= SILENCE_THRESHOLD {
                                silent_packets_run = silent_packets_run.saturating_add(1);
                                if silent_packets_run == hangover_packets {
                                    // Smoothly fade the final tail packet to exact 0.0 before pausing
                                    let len = pkt_slice.len();
                                    for (i, frame) in pkt_slice.iter_mut().enumerate() {
                                        let atten = 1.0 - (i as f32 / len as f32);
                                        frame[0] *= atten;
                                        frame[1] *= atten;
                                    }
                                } else if silent_packets_run > hangover_packets {
                                    if !is_paused {
                                        is_paused = true;
                                        paused_cb.store(true, Ordering::Relaxed);
                                    }
                                    frame_accum.clear();
                                    continue;
                                }
                            } else {
                                if is_paused {
                                    is_paused = false;
                                    paused_cb.store(false, Ordering::Relaxed);
                                    flags |= FLAG_STREAM_RESUMED;
                                }
                                silent_packets_run = 0;
                            }
                        }

                        encode_audio_packet(
                            codec,
                            seq,
                            sample_rate,
                            flags,
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
            }
        })
        .map_err(|e| e.to_string())?;

    print_sender_status_loop(
        codec,
        sample_rate,
        &packets_sent,
        &bytes_sent,
        &peak_bits,
        &stream_paused,
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
    stream_paused: &AtomicBool,
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
        let paused = stream_paused.load(Ordering::Relaxed);

        let target_count = if !static_targets.is_empty() {
            static_targets.len()
        } else {
            receivers.lock().map(|m| m.len()).unwrap_or(0)
        };

        let rate_str = if paused {
            "   0.0 kbps (Silent)".to_string()
        } else {
            format!("{:6.1} kbps        ", kbps)
        };

        print!(
            "\r  Receivers: {} | {} {}Hz | {} | Pkts: {} {}   ",
            target_count, codec, sample_rate, rate_str, pkts, vu
        );
        let _ = std::io::stdout().flush();
    }
}
