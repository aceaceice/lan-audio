use crate::audio::{self, MacOutputGuard, MacRouteMode};
use crate::discovery::{
    local_hostname, local_lan_ipv4, scan_lan_receivers, spawn_receiver_responder,
    spawn_sender_discovery_watcher, DiscoveredReceiver,
};
use crate::dsp::{
    compute_peak_and_rms, decode_audio_packet, encode_audio_packet, AdpcmChannelState, JitterBuffer,
};
use crate::protocol::{
    Codec, DEFAULT_AUDIO_PORT, DEFAULT_DISCOVERY_PORT, DEFAULT_PACKET_FRAMES, FLAG_STREAM_RESUMED,
};
use crate::theme::{RiceTheme, ThemeWatcher};
use eframe::egui::{self, Color32, RichText, Rounding, Sense, Stroke, Vec2};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GuiMode {
    Send,
    Recv,
}

struct ActiveSession {
    running: Arc<AtomicBool>,
    mac_guard: Option<MacOutputGuard>,
    _cpal_stream: Option<cpal::Stream>,
    // Shared telemetry
    peak_bits: Arc<AtomicU32>,
    packets: Arc<AtomicU64>,
    bytes_sent: Arc<AtomicU64>,
    stream_paused: Arc<AtomicBool>,
    // Receiver telemetry
    jitter: Option<Arc<Mutex<JitterBuffer>>>,
    last_sender: Option<Arc<Mutex<Option<(SocketAddr, Codec)>>>>,
    // Sender discovered receivers
    discovered_receivers: Option<Arc<Mutex<HashMap<SocketAddr, DiscoveredReceiver>>>>,
    status_note: String,
}

impl Drop for ActiveSession {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(ref mut g) = self.mac_guard {
            g.restore();
        }
    }
}

pub struct LanAudioGui {
    theme_watcher: ThemeWatcher,
    theme_initialized: bool,
    last_reload_flash: Option<Instant>,

    mode: GuiMode,
    mac_route: MacRouteMode,
    codec: Codec,
    silence_pause: Arc<AtomicBool>,
    use_mic: bool,

    volume: f32,
    volume_bits: Arc<AtomicU32>,
    target_latency_ms: f32,
    max_latency_ms: f32,

    input_devices: Vec<String>,
    output_devices: Vec<String>,
    selected_input_idx: usize,
    selected_output_idx: usize,

    manual_target: String,
    lan_scan_results: Vec<DiscoveredReceiver>,
    scan_rx: Option<mpsc::Receiver<Vec<DiscoveredReceiver>>>,

    session: Option<ActiveSession>,
    error_banner: Option<String>,

    // Bitrate calculation
    prev_bytes: u64,
    last_rate_tick: Instant,
    current_kbps: f64,
}

impl LanAudioGui {
    pub fn new(initial_mode: GuiMode, style_path: Option<PathBuf>) -> Self {
        let theme_watcher = ThemeWatcher::new(style_path);
        let input_devices = audio::list_device_names(true);
        let output_devices = audio::list_device_names(false);

        let mut app = Self {
            theme_watcher,
            theme_initialized: false,
            last_reload_flash: None,
            mode: initial_mode,
            mac_route: MacRouteMode::Auto,
            codec: Codec::PcmI16,
            silence_pause: Arc::new(AtomicBool::new(true)),
            use_mic: false,
            volume: 1.0,
            volume_bits: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            target_latency_ms: 20.0,
            max_latency_ms: 45.0,
            input_devices,
            output_devices,
            selected_input_idx: 0,
            selected_output_idx: 0,
            manual_target: String::new(),
            lan_scan_results: Vec::new(),
            scan_rx: None,
            session: None,
            error_banner: None,
            prev_bytes: 0,
            last_rate_tick: Instant::now(),
            current_kbps: 0.0,
        };
        app.trigger_background_scan();
        app
    }

    fn trigger_background_scan(&mut self) {
        if self.scan_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.scan_rx = Some(rx);
        thread::spawn(move || {
            let found = scan_lan_receivers(DEFAULT_DISCOVERY_PORT, Duration::from_millis(750));
            let _ = tx.send(found);
        });
    }

    fn stop_session(&mut self) {
        if let Some(mut sess) = self.session.take() {
            sess.running.store(false, Ordering::Relaxed);
            if let Some(ref mut g) = sess.mac_guard {
                g.restore();
            }
        }
        self.current_kbps = 0.0;
        self.prev_bytes = 0;
    }

    fn start_session(&mut self) {
        self.stop_session();
        self.error_banner = None;

        match self.mode {
            GuiMode::Send => {
                if let Err(e) = self.start_sender_session() {
                    self.error_banner = Some(e);
                }
            }
            GuiMode::Recv => {
                if let Err(e) = self.start_receiver_session() {
                    self.error_banner = Some(e);
                }
            }
        }
    }

    fn start_sender_session(&mut self) -> Result<(), String> {
        let running = Arc::new(AtomicBool::new(true));
        let mac_guard = MacOutputGuard::activate(self.mac_route);

        let receivers: Arc<Mutex<HashMap<SocketAddr, DiscoveredReceiver>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let mut static_targets = Vec::new();
        let trimmed_target = self.manual_target.trim();
        if !trimmed_target.is_empty() {
            let addr = crate::parse_target_addr(trimmed_target)?;
            static_targets.push(addr);
        } else {
            if let Ok(mut map) = receivers.lock() {
                for r in &self.lan_scan_results {
                    map.insert(r.audio_socket_addr(), r.clone());
                }
            }
            let _ = spawn_sender_discovery_watcher(
                DEFAULT_DISCOVERY_PORT,
                Arc::clone(&receivers),
                Arc::clone(&running),
            );
        }

        let sock = UdpSocket::bind("0.0.0.0:0")
            .map_err(|e| format!("Failed to bind UDP socket: {}", e))?;

        let packets = Arc::new(AtomicU64::new(0));
        let bytes_sent = Arc::new(AtomicU64::new(0));
        let peak_bits = Arc::new(AtomicU32::new(0));
        let stream_paused = Arc::new(AtomicBool::new(false));

        let (audio_tx, audio_rx) = mpsc::sync_channel::<Vec<[f32; 2]>>(64);

        let dev_query = if self.selected_input_idx == 0 {
            None
        } else {
            self.input_devices
                .get(self.selected_input_idx)
                .map(|s| s.trim_start_matches("PulseMonitor: ").to_string())
        };

        let preferred_rate = 48000u32;
        let mut sample_rate = preferred_rate;
        let mut cpal_stream: Option<cpal::Stream> = None;
        let mut pulse_started = false;
        let mut status_note = String::new();

        #[cfg(target_os = "linux")]
        if !self.use_mic && dev_query.is_none() {
            if let Some(monitor_source) = audio::linux_default_pulse_monitor() {
                let tx_pulse = audio_tx.clone();
                if audio::pulse_ffi::spawn_low_latency_capture(
                    &monitor_source,
                    preferred_rate,
                    DEFAULT_PACKET_FRAMES,
                    Arc::clone(&running),
                    move |frames| {
                        let _ = tx_pulse.try_send(frames.to_vec());
                    },
                )
                .is_ok()
                {
                    pulse_started = true;
                    status_note = format!("Capture: {} (48kHz)", monitor_source);
                }
            }
        }

        if !pulse_started {
            let selected =
                audio::select_input_device(dev_query.as_deref(), !self.use_mic, Some(preferred_rate))?;
            sample_rate = selected.config.sample_rate.0;
            status_note = format!("Capture: {} ({}Hz)", selected.name, sample_rate);
            let tx_cpal = audio_tx;
            let stream = audio::build_stereo_input_stream(&selected, move |stereo_chunk| {
                let _ = tx_cpal.try_send(stereo_chunk.to_vec());
            })?;
            cpal_stream = Some(stream);
        }

        if let (Some(orig), Some(act)) = (&mac_guard.original_name, &mac_guard.active_name) {
            if orig != act {
                status_note = format!("{} | Routed: {} -> {}", status_note, orig, act);
            }
        }

        let running_net = Arc::clone(&running);
        let packets_cb = Arc::clone(&packets);
        let bytes_cb = Arc::clone(&bytes_sent);
        let peak_cb = Arc::clone(&peak_bits);
        let paused_cb = Arc::clone(&stream_paused);
        let silence_cfg = Arc::clone(&self.silence_pause);
        let vol_bits = Arc::clone(&self.volume_bits);
        let receivers_cb = Arc::clone(&receivers);
        let codec = self.codec;

        thread::Builder::new()
            .name("gui-udp-send".into())
            .spawn(move || {
                let mut frame_accum: Vec<[f32; 2]> = Vec::with_capacity(DEFAULT_PACKET_FRAMES * 4);
                let mut packet_buf: Vec<u8> = Vec::with_capacity(1500);
                let mut adpcm_states = [AdpcmChannelState::default(), AdpcmChannelState::default()];
                let mut seq: u32 = 0;

                const SILENCE_THRESHOLD: f32 = 0.00008;
                let hangover_packets = ((sample_rate as usize / DEFAULT_PACKET_FRAMES) / 2).max(40);
                let mut silent_packets_run: usize = 0;
                let mut is_paused = false;

                while running_net.load(Ordering::Relaxed) {
                    let Ok(chunk) = audio_rx.recv_timeout(Duration::from_millis(100)) else {
                        continue;
                    };
                    let volume = f32::from_bits(vol_bits.load(Ordering::Relaxed));
                    let (peak, _) = compute_peak_and_rms(&chunk);
                    peak_cb.store((peak * volume).to_bits(), Ordering::Relaxed);

                    let dtx_enabled = silence_cfg.load(Ordering::Relaxed);

                    for &[l, r] in &chunk {
                        frame_accum.push([l * volume, r * volume]);
                        if frame_accum.len() >= DEFAULT_PACKET_FRAMES {
                            let pkt_slice = &mut frame_accum[..DEFAULT_PACKET_FRAMES];
                            let (pkt_peak, _) = compute_peak_and_rms(pkt_slice);

                            let mut flags = 0u8;
                            if dtx_enabled {
                                if pkt_peak <= SILENCE_THRESHOLD {
                                    silent_packets_run = silent_packets_run.saturating_add(1);
                                    if silent_packets_run == hangover_packets {
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
                            } else if is_paused {
                                is_paused = false;
                                paused_cb.store(false, Ordering::Relaxed);
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

                            crate::send_packet_to_targets(
                                &sock,
                                &packet_buf,
                                &static_targets,
                                &receivers_cb,
                                &packets_cb,
                                &bytes_cb,
                            );
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;

        self.session = Some(ActiveSession {
            running,
            mac_guard: Some(mac_guard),
            _cpal_stream: cpal_stream,
            peak_bits,
            packets,
            bytes_sent,
            stream_paused,
            jitter: None,
            last_sender: None,
            discovered_receivers: Some(receivers),
            status_note,
        });
        Ok(())
    }

    fn start_receiver_session(&mut self) -> Result<(), String> {
        let running = Arc::new(AtomicBool::new(true));
        let output_rate = 48000u32;
        let jitter = Arc::new(Mutex::new(JitterBuffer::new(
            output_rate,
            self.target_latency_ms,
            Some(self.max_latency_ms),
        )));
        let peak_bits = Arc::new(AtomicU32::new(0));
        let packets = Arc::new(AtomicU64::new(0));

        let dev_query = if self.selected_output_idx == 0 {
            None
        } else {
            self.output_devices
                .get(self.selected_output_idx)
                .map(|s| {
                    if s.starts_with("pulse") {
                        "pulse".to_string()
                    } else {
                        s.clone()
                    }
                })
        };

        let mut cpal_stream: Option<cpal::Stream> = None;
        let mut pulse_started = false;
        let mut actual_rate = output_rate;
        let mut status_note = String::new();
        let vol_bits = Arc::clone(&self.volume_bits);

        #[cfg(target_os = "linux")]
        if dev_query.is_none() || dev_query.as_deref() == Some("pulse") {
            let jitter_out = Arc::clone(&jitter);
            let peak_out = Arc::clone(&peak_bits);
            let vol_out = Arc::clone(&vol_bits);
            if audio::pulse_ffi::spawn_low_latency_playback(
                None,
                output_rate,
                self.target_latency_ms,
                Arc::clone(&running),
                move |out_stereo| {
                    let volume = f32::from_bits(vol_out.load(Ordering::Relaxed));
                    if let Ok(mut jb) = jitter_out.lock() {
                        jb.pop_output_frames(out_stereo, volume);
                    } else {
                        out_stereo.fill([0.0, 0.0]);
                    }
                    let (peak, _) = compute_peak_and_rms(out_stereo);
                    peak_out.store(peak.to_bits(), Ordering::Relaxed);
                },
            )
            .is_ok()
            {
                pulse_started = true;
                status_note = "Output: PulseAudio Low-Latency Direct (48kHz)".to_string();
            }
        }

        if !pulse_started {
            let selected = audio::select_output_device(dev_query.as_deref(), Some(48000))?;
            actual_rate = selected.config.sample_rate.0;
            if let Ok(mut jb) = jitter.lock() {
                *jb = JitterBuffer::new(
                    actual_rate,
                    self.target_latency_ms,
                    Some(self.max_latency_ms),
                );
            }
            status_note = format!("Output: {} ({}Hz)", selected.name, actual_rate);
            let jitter_out = Arc::clone(&jitter);
            let peak_out = Arc::clone(&peak_bits);
            let vol_out = Arc::clone(&vol_bits);
            let stream = audio::build_stereo_output_stream(&selected, move |out_stereo| {
                let volume = f32::from_bits(vol_out.load(Ordering::Relaxed));
                if let Ok(mut jb) = jitter_out.lock() {
                    jb.pop_output_frames(out_stereo, volume);
                } else {
                    out_stereo.fill([0.0, 0.0]);
                }
                let (peak, _) = compute_peak_and_rms(out_stereo);
                peak_out.store(peak.to_bits(), Ordering::Relaxed);
            })?;
            cpal_stream = Some(stream);
        }

        let _ = spawn_receiver_responder(
            DEFAULT_DISCOVERY_PORT,
            local_hostname(),
            DEFAULT_AUDIO_PORT,
            actual_rate,
            2,
            Arc::clone(&running),
        );

        let sock = UdpSocket::bind(("0.0.0.0", DEFAULT_AUDIO_PORT))
            .map_err(|e| format!("Port {} busy or unavailable: {}", DEFAULT_AUDIO_PORT, e))?;
        let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));

        let last_sender: Arc<Mutex<Option<(SocketAddr, Codec)>>> = Arc::new(Mutex::new(None));
        let last_sender_net = Arc::clone(&last_sender);
        let jitter_net = Arc::clone(&jitter);
        let running_net = Arc::clone(&running);
        let pkts_net = Arc::clone(&packets);

        thread::Builder::new()
            .name("gui-udp-recv".into())
            .spawn(move || {
                let mut packet_buf = [0u8; 4096];
                let mut decoded_frames = Vec::with_capacity(512);
                while running_net.load(Ordering::Relaxed) {
                    if let Ok((len, src)) = sock.recv_from(&mut packet_buf) {
                        if let Some(hdr) =
                            decode_audio_packet(&packet_buf[..len], &mut decoded_frames)
                        {
                            if let Ok(mut ls) = last_sender_net.lock() {
                                *ls = Some((src, hdr.codec));
                            }
                            if let Ok(mut jb) = jitter_net.lock() {
                                jb.push_packet(&hdr, &decoded_frames);
                                pkts_net.store(jb.packets_received, Ordering::Relaxed);
                            }
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;

        self.session = Some(ActiveSession {
            running,
            mac_guard: None,
            _cpal_stream: cpal_stream,
            peak_bits,
            packets,
            bytes_sent: Arc::new(AtomicU64::new(0)),
            stream_paused: Arc::new(AtomicBool::new(false)),
            jitter: Some(jitter),
            last_sender: Some(last_sender),
            discovered_receivers: None,
            status_note,
        });
        Ok(())
    }
}

fn draw_pill(ui: &mut egui::Ui, text: &str, bg: Color32, fg: Color32, rounding: f32) {
    egui::Frame::none()
        .fill(bg)
        .rounding(Rounding::same(rounding))
        .inner_margin(egui::Margin::symmetric(8.0, 3.0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).monospace().strong().size(11.5).color(fg));
        });
}

fn draw_segmented_vu(ui: &mut egui::Ui, peak: f32, t: &RiceTheme) {
    let db = if peak <= 0.0001 {
        -60.0
    } else {
        20.0 * peak.log10()
    };
    let norm = ((db + 55.0) / 55.0).clamp(0.0, 1.0);
    let segments = 28usize;
    let filled = (norm * segments as f32).round() as usize;

    let desired = Vec2::new(ui.available_width(), 18.0);
    let (rect, _) = ui.allocate_exact_size(desired, Sense::hover());
    let painter = ui.painter();

    painter.rect_filled(rect, Rounding::same(t.border_radius), Color32::from_rgb(0x1d, 0x20, 0x21));
    painter.rect_stroke(
        rect,
        Rounding::same(t.border_radius),
        Stroke::new(t.border_width, t.unfocused),
    );

    let pad = 3.0;
    let inner = rect.shrink(pad);
    let gap = 2.0;
    let seg_w = ((inner.width() - gap * (segments as f32 - 1.0)) / segments as f32).max(2.0);

    for i in 0..segments {
        let x = inner.left() + i as f32 * (seg_w + gap);
        let seg_rect = egui::Rect::from_min_size(egui::pos2(x, inner.top()), Vec2::new(seg_w, inner.height()));
        let frac = i as f32 / segments as f32;
        let color = if i < filled {
            if frac > 0.85 {
                t.critical
            } else if frac > 0.68 {
                t.warning
            } else {
                t.memory
            }
        } else {
            t.bg2.linear_multiply(0.45)
        };
        painter.rect_filled(seg_rect, Rounding::same(t.border_radius * 0.5), color);
    }
}

impl eframe::App for LanAudioGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.theme_initialized {
            self.theme_watcher.theme.apply_to_egui(ctx);
            self.theme_initialized = true;
        }
        if self.theme_watcher.poll_reload(ctx) {
            self.last_reload_flash = Some(Instant::now());
        }

        if let Some(rx) = &self.scan_rx {
            if let Ok(found) = rx.try_recv() {
                self.lan_scan_results = found;
                self.scan_rx = None;
            }
        }

        // Update bitrate every 250ms
        if self.last_rate_tick.elapsed() >= Duration::from_millis(250) {
            let dt = self.last_rate_tick.elapsed().as_secs_f64().max(0.001);
            self.last_rate_tick = Instant::now();
            if let Some(ref sess) = self.session {
                let total = sess.bytes_sent.load(Ordering::Relaxed);
                self.current_kbps =
                    ((total.saturating_sub(self.prev_bytes) as f64) * 8.0 / 1000.0) / dt;
                self.prev_bytes = total;
            }
        }

        let t = self.theme_watcher.theme.clone();
        let is_active = self.session.is_some();

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(t.black)
                    .inner_margin(egui::Margin::same(12.0)),
            )
            .show(ctx, |ui| {
                // === TOP WAYBAR-STYLE HEADER ===
                ui.horizontal(|ui| {
                    draw_pill(ui, "LAN-AUDIO", t.layout, t.date, t.border_radius);

                    let host_ip = local_lan_ipv4()
                        .map(|ip| ip.to_string())
                        .unwrap_or_else(|| "127.0.0.1".into());
                    draw_pill(
                        ui,
                        &format!("{} @ {}", local_hostname(), host_ip),
                        t.bg2,
                        t.time,
                        t.border_radius,
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(ref sess) = self.session {
                            let paused = sess.stream_paused.load(Ordering::Relaxed);
                            if paused {
                                draw_pill(ui, "SILENT / PAUSED (0 kbps)", t.warning, t.black, t.border_radius);
                            } else {
                                draw_pill(ui, "LIVE STREAMING", t.temp, t.black, t.border_radius);
                            }
                        } else {
                            draw_pill(ui, "STOPPED", t.unfocused, t.gray, t.border_radius);
                        }

                        if let Some(flash) = self.last_reload_flash {
                            if flash.elapsed() < Duration::from_secs(2) {
                                draw_pill(ui, "CSS RELOADED", t.focused, t.black, t.border_radius);
                            }
                        }
                    });
                });

                ui.add_space(10.0);

                // === MODE SELECTOR BAR ===
                egui::Frame::none()
                    .fill(t.bg2.linear_multiply(0.35))
                    .stroke(Stroke::new(t.border_width, t.unfocused))
                    .rounding(Rounding::same(t.border_radius))
                    .inner_margin(egui::Margin::same(8.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("MODE")
                                    .monospace()
                                    .strong()
                                    .color(t.layout),
                            );
                            ui.add_space(6.0);

                            let send_selected = self.mode == GuiMode::Send;
                            let send_btn = egui::Button::new(
                                RichText::new(" [ SENDER (TX) ] ")
                                    .monospace()
                                    .strong()
                                    .color(if send_selected { t.black } else { t.time }),
                            )
                            .fill(if send_selected { t.focused } else { t.bg2 });
                            if ui.add(send_btn).clicked() && self.mode != GuiMode::Send {
                                self.stop_session();
                                self.mode = GuiMode::Send;
                            }

                            let recv_selected = self.mode == GuiMode::Recv;
                            let recv_btn = egui::Button::new(
                                RichText::new(" [ RECEIVER (RX) ] ")
                                    .monospace()
                                    .strong()
                                    .color(if recv_selected { t.black } else { t.time }),
                            )
                            .fill(if recv_selected { t.focused } else { t.bg2 });
                            if ui.add(recv_btn).clicked() && self.mode != GuiMode::Recv {
                                self.stop_session();
                                self.mode = GuiMode::Recv;
                            }
                        });
                    });

                ui.add_space(8.0);

                if let Some(ref err) = self.error_banner {
                    egui::Frame::none()
                        .fill(t.critical.linear_multiply(0.25))
                        .stroke(Stroke::new(t.border_width, t.critical))
                        .rounding(Rounding::same(t.border_radius))
                        .inner_margin(egui::Margin::same(8.0))
                        .show(ui, |ui| {
                            ui.label(RichText::new(format!("[!] {}", err)).monospace().color(t.brred));
                        });
                    ui.add_space(8.0);
                }

                // === NETWORK / DISCOVERY MODULE ===
                egui::Frame::none()
                    .fill(t.black)
                    .stroke(Stroke::new(t.border_width, t.network))
                    .rounding(Rounding::same(t.border_radius))
                    .inner_margin(egui::Margin::same(10.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            draw_pill(ui, "NETWORK", t.network, t.white, t.border_radius);
                            ui.label(
                                RichText::new(format!("UDP :{} / :{}", DEFAULT_DISCOVERY_PORT, DEFAULT_AUDIO_PORT))
                                    .monospace()
                                    .size(11.5)
                                    .color(t.gray),
                            );
                            if self.mode == GuiMode::Send {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    let scanning = self.scan_rx.is_some();
                                    let label = if scanning { "SCANNING..." } else { "RESCAN LAN" };
                                    if ui
                                        .add_enabled(!scanning, egui::Button::new(RichText::new(label).monospace().size(11.0)))
                                        .clicked()
                                    {
                                        self.trigger_background_scan();
                                    }
                                });
                            }
                        });

                        ui.add_space(6.0);

                        if self.mode == GuiMode::Send {
                            let active_receivers: Vec<DiscoveredReceiver> = if let Some(ref sess) = self.session {
                                if let Some(ref map_arc) = sess.discovered_receivers {
                                    map_arc
                                        .lock()
                                        .map(|m| m.values().cloned().collect())
                                        .unwrap_or_default()
                                } else {
                                    self.lan_scan_results.clone()
                                }
                            } else {
                                self.lan_scan_results.clone()
                            };

                            if active_receivers.is_empty() {
                                ui.label(
                                    RichText::new("  No receivers detected yet (auto-discovers when `lan-audio recv` runs)")
                                        .monospace()
                                        .size(11.5)
                                        .color(t.brgray),
                                );
                            } else {
                                for r in &active_receivers {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new("●").monospace().color(t.temp));
                                        ui.label(
                                            RichText::new(format!(
                                                "{} ({}:{}) [{}Hz]",
                                                r.hostname, r.ip, r.audio_port, r.sample_rate
                                            ))
                                            .monospace()
                                            .color(t.white),
                                        );
                                    });
                                }
                            }

                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Target IP (blank = auto):").monospace().size(11.5).color(t.gray));
                                ui.add_enabled(
                                    !is_active,
                                    egui::TextEdit::singleline(&mut self.manual_target)
                                        .desired_width(180.0)
                                        .font(egui::TextStyle::Monospace),
                                );
                            });
                        } else {
                            let conn_text = if let Some(ref sess) = self.session {
                                if let Some(ref ls) = sess.last_sender {
                                    if let Ok(guard) = ls.lock() {
                                        if let Some((addr, codec)) = *guard {
                                            format!("Connected sender: {} ({})", addr, codec)
                                        } else {
                                            "Waiting for sender stream on LAN...".to_string()
                                        }
                                    } else {
                                        "Waiting...".to_string()
                                    }
                                } else {
                                    "Receiver ready".to_string()
                                }
                            } else {
                                "Click START below to advertise receiver on LAN".to_string()
                            };
                            ui.label(RichText::new(conn_text).monospace().color(t.white));
                        }
                    });

                ui.add_space(8.0);

                // === AUDIO & MACOS ROUTING MODULE ===
                egui::Frame::none()
                    .fill(t.black)
                    .stroke(Stroke::new(t.border_width, t.sound))
                    .rounding(Rounding::same(t.border_radius))
                    .inner_margin(egui::Margin::same(10.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            draw_pill(ui, "AUDIO & ROUTING", t.sound, t.black, t.border_radius);
                            if let Some(ref sess) = self.session {
                                ui.label(
                                    RichText::new(&sess.status_note)
                                        .monospace()
                                        .size(11.0)
                                        .color(t.gray),
                                );
                            }
                        });

                        ui.add_space(6.0);

                        if self.mode == GuiMode::Send {
                            // macOS Automatic Output Device Switcher
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new("macOS Output Route:")
                                        .monospace()
                                        .size(11.5)
                                        .color(t.gray),
                                );
                                for mode_opt in [
                                    MacRouteMode::Auto,
                                    MacRouteMode::MultiOutput,
                                    MacRouteMode::PcOnly,
                                    MacRouteMode::Manual,
                                ] {
                                    let sel = self.mac_route == mode_opt;
                                    let btn = egui::Button::new(
                                        RichText::new(mode_opt.label())
                                            .monospace()
                                            .size(11.0)
                                            .color(if sel { t.black } else { t.time }),
                                    )
                                    .fill(if sel { t.sound } else { t.bg2 });
                                    if ui.add(btn).clicked() {
                                        self.mac_route = mode_opt;
                                    }
                                }
                            });

                            ui.add_space(4.0);

                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Input Device:").monospace().size(11.5).color(t.gray));
                                egui::ComboBox::from_id_salt("input_dev_combo")
                                    .selected_text(
                                        RichText::new(
                                            self.input_devices
                                                .get(self.selected_input_idx)
                                                .map(|s| s.as_str())
                                                .unwrap_or("Default"),
                                        )
                                        .monospace()
                                        .size(11.5),
                                    )
                                    .show_ui(ui, |ui| {
                                        for (idx, name) in self.input_devices.iter().enumerate() {
                                            ui.selectable_value(
                                                &mut self.selected_input_idx,
                                                idx,
                                                RichText::new(name).monospace(),
                                            );
                                        }
                                    });
                            });

                            ui.add_space(4.0);

                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Codec:").monospace().size(11.5).color(t.gray));
                                for c in [Codec::PcmI16, Codec::Adpcm, Codec::PcmF32] {
                                    let sel = self.codec == c;
                                    let b = egui::Button::new(
                                        RichText::new(format!("{}", c))
                                            .monospace()
                                            .size(11.0)
                                            .color(if sel { t.black } else { t.time }),
                                    )
                                    .fill(if sel { t.focused } else { t.bg2 });
                                    if ui.add_enabled(!is_active, b).clicked() {
                                        self.codec = c;
                                    }
                                }

                                ui.add_space(12.0);
                                let mut dtx = self.silence_pause.load(Ordering::Relaxed);
                                if ui
                                    .checkbox(
                                        &mut dtx,
                                        RichText::new("Auto-Pause on Silence (DTX)")
                                            .monospace()
                                            .size(11.5),
                                    )
                                    .changed()
                                {
                                    self.silence_pause.store(dtx, Ordering::Relaxed);
                                }
                            });
                        } else {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Output Device:").monospace().size(11.5).color(t.gray));
                                egui::ComboBox::from_id_salt("output_dev_combo")
                                    .selected_text(
                                        RichText::new(
                                            self.output_devices
                                                .get(self.selected_output_idx)
                                                .map(|s| s.as_str())
                                                .unwrap_or("Default"),
                                        )
                                        .monospace()
                                        .size(11.5),
                                    )
                                    .show_ui(ui, |ui| {
                                        for (idx, name) in self.output_devices.iter().enumerate() {
                                            ui.selectable_value(
                                                &mut self.selected_output_idx,
                                                idx,
                                                RichText::new(name).monospace(),
                                            );
                                        }
                                    });
                            });

                            ui.add_space(4.0);

                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Target Buf (ms):").monospace().size(11.5).color(t.gray));
                                if ui
                                    .add_enabled(
                                        !is_active,
                                        egui::Slider::new(&mut self.target_latency_ms, 8.0..=80.0)
                                            .step_by(1.0),
                                    )
                                    .changed()
                                {
                                    if self.max_latency_ms < self.target_latency_ms + 15.0 {
                                        self.max_latency_ms = (self.target_latency_ms * 2.25).max(self.target_latency_ms + 20.0);
                                    }
                                }

                                ui.label(RichText::new("Cap (ms):").monospace().size(11.5).color(t.gray));
                                ui.add_enabled(
                                    !is_active,
                                    egui::Slider::new(&mut self.max_latency_ms, 20.0..=150.0)
                                        .step_by(1.0),
                                );
                            });
                        }

                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Gain / Volume:").monospace().size(11.5).color(t.gray));
                            if ui
                                .add(
                                    egui::Slider::new(&mut self.volume, 0.0..=2.0)
                                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                                )
                                .changed()
                            {
                                self.volume_bits.store(self.volume.to_bits(), Ordering::Relaxed);
                            }
                        });
                    });

                ui.add_space(8.0);

                // === TELEMETRY & VU METER MODULE ===
                egui::Frame::none()
                    .fill(t.black)
                    .stroke(Stroke::new(t.border_width, t.memory))
                    .rounding(Rounding::same(t.border_radius))
                    .inner_margin(egui::Margin::same(10.0))
                    .show(ui, |ui| {
                        let (peak, pkts, buf_str, loss_str) = if let Some(ref sess) = self.session {
                            let p = f32::from_bits(sess.peak_bits.load(Ordering::Relaxed));
                            let pk = sess.packets.load(Ordering::Relaxed);
                            if let Some(ref jb_arc) = sess.jitter {
                                if let Ok(jb) = jb_arc.lock() {
                                    let b = if jb.is_active() {
                                        format!("{:4.1} ms", jb.current_latency_ms())
                                    } else {
                                        "IDLE".to_string()
                                    };
                                    let l = format!("Lost: {} | Underrun: {}", jb.packets_lost, jb.underruns);
                                    (p, pk, b, l)
                                } else {
                                    (p, pk, "--".into(), "".into())
                                }
                            } else {
                                let paused = sess.stream_paused.load(Ordering::Relaxed);
                                let b = if paused {
                                    "0.0 kbps (SILENT)".to_string()
                                } else {
                                    format!("{:.1} kbps", self.current_kbps)
                                };
                                (p, pk, b, format!("Codec: {}", self.codec))
                            }
                        } else {
                            (0.0, 0, "--".into(), "Idle".into())
                        };

                        ui.horizontal(|ui| {
                            draw_pill(ui, "TELEMETRY", t.memory, t.black, t.border_radius);
                            draw_pill(
                                ui,
                                &format!(
                                    "{}: {}",
                                    if self.mode == GuiMode::Recv { "BUF" } else { "RATE" },
                                    buf_str
                                ),
                                t.bg2,
                                t.focused,
                                t.border_radius,
                            );
                            draw_pill(ui, &format!("PKTS: {}", pkts), t.bg2, t.time, t.border_radius);
                            if !loss_str.is_empty() {
                                ui.label(RichText::new(loss_str).monospace().size(11.0).color(t.gray));
                            }
                        });

                        ui.add_space(6.0);
                        draw_segmented_vu(ui, peak, &t);
                    });

                ui.add_space(10.0);

                // === START / STOP ACTION BUTTON ===
                let btn_height = 36.0;
                if is_active {
                    let stop_btn = egui::Button::new(
                        RichText::new("■  STOP STREAM  (RESTORE AUDIO ROUTE)")
                            .monospace()
                            .strong()
                            .size(13.5)
                            .color(t.white),
                    )
                    .fill(t.critical)
                    .min_size(Vec2::new(ui.available_width(), btn_height));
                    if ui.add(stop_btn).clicked() {
                        self.stop_session();
                    }
                } else {
                    let start_label = match self.mode {
                        GuiMode::Send => "▶  START STREAMING TO LAN",
                        GuiMode::Recv => "▶  START LAN RECEIVER",
                    };
                    let start_btn = egui::Button::new(
                        RichText::new(start_label)
                            .monospace()
                            .strong()
                            .size(13.5)
                            .color(t.black),
                    )
                    .fill(t.temp)
                    .min_size(Vec2::new(ui.available_width(), btn_height));
                    if ui.add(start_btn).clicked() {
                        self.start_session();
                    }
                }

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "Theme CSS (hot-reloads live): {}",
                            self.theme_watcher.path.display()
                        ))
                        .monospace()
                        .size(10.5)
                        .color(t.brgray),
                    );
                });
            });

        // Repaint at ~30 FPS when active for smooth VU meter, or every 250ms when idle
        if is_active {
            ctx.request_repaint_after(Duration::from_millis(33));
        } else {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_session();
    }
}

pub fn run_gui(initial_mode: GuiMode, style_path: Option<PathBuf>) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("lan-audio")
            .with_inner_size([560.0, 460.0])
            .with_min_inner_size([480.0, 410.0]),
        ..Default::default()
    };

    eframe::run_native(
        "lan-audio",
        options,
        Box::new(move |_cc| Ok(Box::new(LanAudioGui::new(initial_mode, style_path)))),
    )
    .map_err(|e| format!("Failed to launch GUI: {}", e))
}
