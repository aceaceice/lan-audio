use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Device, SampleFormat, Stream, StreamConfig};
use std::process::Command;

pub struct SelectedDevice {
    pub device: Device,
    pub name: String,
    pub config: StreamConfig,
    pub sample_format: SampleFormat,
}

const MACOS_LOOPBACK_KEYWORDS: &[&str] = &[
    "blackhole",
    "loopback",
    "soundflower",
    "vb-cable",
    "existential",
    "multi-output",
];

#[cfg(target_os = "linux")]
pub mod pulse_ffi {
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::ptr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};

    #[repr(C)]
    pub struct PaSimple {
        _private: [u8; 0],
    }

    pub const PA_STREAM_PLAYBACK: c_int = 1;
    pub const PA_STREAM_RECORD: c_int = 2;
    pub const PA_SAMPLE_FLOAT32LE: c_int = 5;

    #[repr(C)]
    pub struct PaSampleSpec {
        pub format: c_int,
        pub rate: u32,
        pub channels: u8,
    }

    #[repr(C)]
    pub struct PaBufferAttr {
        pub maxlength: u32,
        pub tlength: u32,
        pub prebuf: u32,
        pub minreq: u32,
        pub fragsize: u32,
    }

    #[link(name = "pulse-simple")]
    #[link(name = "pulse")]
    extern "C" {
        pub fn pa_simple_new(
            server: *const c_char,
            name: *const c_char,
            dir: c_int,
            dev: *const c_char,
            stream_name: *const c_char,
            ss: *const PaSampleSpec,
            map: *const c_void,
            attr: *const PaBufferAttr,
            error: *mut c_int,
        ) -> *mut PaSimple;

        pub fn pa_simple_write(
            s: *mut PaSimple,
            data: *const c_void,
            bytes: usize,
            error: *mut c_int,
        ) -> c_int;

        pub fn pa_simple_read(
            s: *mut PaSimple,
            data: *mut c_void,
            bytes: usize,
            error: *mut c_int,
        ) -> c_int;

        pub fn pa_simple_free(s: *mut PaSimple);
    }

    /// Spawn a low-latency native PulseAudio/PipeWire playback thread (10ms server buffer, 2.5ms chunks)
    pub fn spawn_low_latency_playback<F>(
        sink_name: Option<&str>,
        sample_rate: u32,
        target_latency_ms: f32,
        running: Arc<AtomicBool>,
        mut fill_frames: F,
    ) -> Result<JoinHandle<()>, String>
    where
        F: FnMut(&mut [[f32; 2]]) + Send + 'static,
    {
        let app_name = CString::new("lan-audio").unwrap();
        let stream_name = CString::new("LAN Audio Stream").unwrap();
        let dev_cstr = sink_name.and_then(|s| CString::new(s).ok());
        let dev_ptr = dev_cstr
            .as_ref()
            .map(|c| c.as_ptr())
            .unwrap_or(ptr::null());

        let ss = PaSampleSpec {
            format: PA_SAMPLE_FLOAT32LE,
            rate: sample_rate,
            channels: 2,
        };

        // 8 bytes per stereo f32 frame
        let bytes_per_frame = 8u32;
        let chunk_frames = 120usize; // 2.5ms @ 48kHz
        let minreq_bytes = (chunk_frames as u32) * bytes_per_frame;
        let target_server_ms = (target_latency_ms * 0.5).clamp(8.0, 40.0);
        let tlength_frames = ((sample_rate as f32 * target_server_ms * 0.001) as u32).max(360);
        let tlength_bytes = tlength_frames * bytes_per_frame;

        let attr = PaBufferAttr {
            maxlength: tlength_bytes * 4,
            tlength: tlength_bytes,
            prebuf: minreq_bytes * 2,
            minreq: minreq_bytes,
            fragsize: u32::MAX,
        };

        let mut err: c_int = 0;
        let pa = unsafe {
            pa_simple_new(
                ptr::null(),
                app_name.as_ptr(),
                PA_STREAM_PLAYBACK,
                dev_ptr,
                stream_name.as_ptr(),
                &ss,
                ptr::null(),
                &attr,
                &mut err,
            )
        };
        if pa.is_null() {
            return Err(format!("pa_simple_new (playback) failed with code {}", err));
        }

        let pa_addr = pa as usize;
        let handle = thread::Builder::new()
            .name("pulse-lowlat-play".into())
            .spawn(move || {
                let pa_ptr = pa_addr as *mut PaSimple;
                let mut buf = vec![[0.0f32; 2]; chunk_frames];
                let byte_len = chunk_frames * 8;
                let mut err_code: c_int = 0;

                while running.load(Ordering::Relaxed) {
                    fill_frames(&mut buf);
                    let rc = unsafe {
                        pa_simple_write(
                            pa_ptr,
                            buf.as_ptr() as *const c_void,
                            byte_len,
                            &mut err_code,
                        )
                    };
                    if rc < 0 {
                        eprintln!("\n[!] PulseAudio write error: {}", err_code);
                        break;
                    }
                }
                unsafe {
                    pa_simple_free(pa_ptr);
                }
            })
            .map_err(|e| e.to_string())?;

        Ok(handle)
    }

    /// Spawn a low-latency native PulseAudio/PipeWire capture thread (2.5ms fragments)
    pub fn spawn_low_latency_capture<F>(
        source_name: &str,
        sample_rate: u32,
        chunk_frames: usize,
        running: Arc<AtomicBool>,
        mut on_frames: F,
    ) -> Result<JoinHandle<()>, String>
    where
        F: FnMut(&[[f32; 2]]) + Send + 'static,
    {
        let app_name = CString::new("lan-audio").unwrap();
        let stream_name = CString::new("LAN Audio Capture").unwrap();
        let dev_cstr = CString::new(source_name).map_err(|e| e.to_string())?;

        let ss = PaSampleSpec {
            format: PA_SAMPLE_FLOAT32LE,
            rate: sample_rate,
            channels: 2,
        };

        let frag_bytes = (chunk_frames as u32) * 8;
        let attr = PaBufferAttr {
            maxlength: frag_bytes * 8,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: frag_bytes,
        };

        let mut err: c_int = 0;
        let pa = unsafe {
            pa_simple_new(
                ptr::null(),
                app_name.as_ptr(),
                PA_STREAM_RECORD,
                dev_cstr.as_ptr(),
                stream_name.as_ptr(),
                &ss,
                ptr::null(),
                &attr,
                &mut err,
            )
        };
        if pa.is_null() {
            return Err(format!("pa_simple_new (capture) failed with code {}", err));
        }

        let pa_addr = pa as usize;
        let handle = thread::Builder::new()
            .name("pulse-lowlat-cap".into())
            .spawn(move || {
                let pa_ptr = pa_addr as *mut PaSimple;
                let mut buf = vec![[0.0f32; 2]; chunk_frames];
                let byte_len = chunk_frames * 8;
                let mut err_code: c_int = 0;

                while running.load(Ordering::Relaxed) {
                    let rc = unsafe {
                        pa_simple_read(
                            pa_ptr,
                            buf.as_mut_ptr() as *mut c_void,
                            byte_len,
                            &mut err_code,
                        )
                    };
                    if rc < 0 {
                        eprintln!("\n[!] PulseAudio read error: {}", err_code);
                        break;
                    }
                    on_frames(&buf);
                }
                unsafe {
                    pa_simple_free(pa_ptr);
                }
            })
            .map_err(|e| e.to_string())?;

        Ok(handle)
    }
}

/// List all available input and output audio devices on the current host
pub fn list_devices() {
    let host = cpal::default_host();
    println!("=== Audio Host: {:?} ===", host.id());

    #[cfg(target_os = "linux")]
    if let Some(monitor) = linux_default_pulse_monitor() {
        println!("\n[Linux PulseAudio/PipeWire Desktop Loopback]");
        println!("  * {} (Default desktop audio monitor)", monitor);
    }

    println!("\n[Input Devices (Capture / Send)]");
    let default_in = host.default_input_device().and_then(|d| d.name().ok());
    if let Ok(devices) = host.input_devices() {
        for (idx, dev) in devices.enumerate() {
            let name = dev.name().unwrap_or_else(|_| "<unknown>".into());
            let is_def = default_in.as_deref() == Some(name.as_str());
            let cfg_str = dev
                .default_input_config()
                .map(|c| {
                    format!(
                        "{} Hz, {}ch, {:?}",
                        c.sample_rate().0,
                        c.channels(),
                        c.sample_format()
                    )
                })
                .unwrap_or_else(|_| "config unavailable".into());
            println!(
                "  [{}] {}{} ({})",
                idx,
                name,
                if is_def { " [DEFAULT]" } else { "" },
                cfg_str
            );
        }
    }

    println!("\n[Output Devices (Playback / Receive)]");
    let default_out = host.default_output_device().and_then(|d| d.name().ok());
    if let Ok(devices) = host.output_devices() {
        for (idx, dev) in devices.enumerate() {
            let name = dev.name().unwrap_or_else(|_| "<unknown>".into());
            let is_def = default_out.as_deref() == Some(name.as_str());
            let cfg_str = dev
                .default_output_config()
                .map(|c| {
                    format!(
                        "{} Hz, {}ch, {:?}",
                        c.sample_rate().0,
                        c.channels(),
                        c.sample_format()
                    )
                })
                .unwrap_or_else(|_| "config unavailable".into());
            println!(
                "  [{}] {}{} ({})",
                idx,
                name,
                if is_def { " [DEFAULT]" } else { "" },
                cfg_str
            );
        }
    }
}

/// On Linux, detect the default PulseAudio/PipeWire sink's `.monitor` source for desktop audio capture
pub fn linux_default_pulse_monitor() -> Option<String> {
    let out = Command::new("pactl")
        .arg("get-default-sink")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sink = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sink.is_empty() {
        None
    } else {
        Some(format!("{}.monitor", sink))
    }
}

pub fn select_input_device(
    requested_name: Option<&str>,
    prefer_loopback: bool,
    preferred_rate: Option<u32>,
) -> Result<SelectedDevice, String> {
    let host = cpal::default_host();

    let device = if let Some(query) = requested_name {
        let q_lower = query.to_lowercase();
        let mut matched = None;
        if let Ok(devices) = host.input_devices() {
            for (idx, d) in devices.enumerate() {
                let name = d.name().unwrap_or_default();
                if idx.to_string() == query || name.to_lowercase().contains(&q_lower) {
                    matched = Some(d);
                    break;
                }
            }
        }
        matched.ok_or_else(|| {
            format!(
                "Input device matching '{}' not found. Run `lan-audio devices` to list options.",
                query
            )
        })?
    } else if prefer_loopback {
        // On macOS, automatically look for BlackHole / Loopback / Soundflower first
        let mut loopback_dev = None;
        if let Ok(devices) = host.input_devices() {
            for d in devices {
                let name_lower = d.name().unwrap_or_default().to_lowercase();
                if MACOS_LOOPBACK_KEYWORDS
                    .iter()
                    .any(|kw| name_lower.contains(kw))
                {
                    loopback_dev = Some(d);
                    break;
                }
            }
        }
        if let Some(d) = loopback_dev {
            d
        } else {
            #[cfg(target_os = "macos")]
            {
                eprintln!(
                    "Note: No virtual loopback driver (like BlackHole) detected on macOS.\n\
                     To capture macOS desktop audio instead of microphone:\n  \
                     1. Run `brew install blackhole-2ch`\n  \
                     2. Select 'BlackHole 2ch' (or a Multi-Output Device) in macOS Sound Settings.\n\
                     Falling back to default input device for now..."
                );
            }
            host.default_input_device()
                .ok_or_else(|| "No default input audio device found".to_string())?
        }
    } else {
        host.default_input_device()
            .ok_or_else(|| "No default input audio device found".to_string())?
    };

    let name = device.name().unwrap_or_else(|_| "default".into());
    let default_cfg = device
        .default_input_config()
        .map_err(|e| format!("Failed to query input config for '{}': {}", name, e))?;

    let mut sample_format = default_cfg.sample_format();
    let mut config: StreamConfig = default_cfg.into();

    if let Some(target_rate) = preferred_rate {
        if let Ok(ranges) = device.supported_input_configs() {
            let best = ranges
                .filter(|r| {
                    r.min_sample_rate().0 <= target_rate
                        && r.max_sample_rate().0 >= target_rate
                        && r.channels() == 2
                        && matches!(r.sample_format(), SampleFormat::F32 | SampleFormat::I16)
                })
                .max_by_key(|r| match r.sample_format() {
                    SampleFormat::F32 => 2,
                    SampleFormat::I16 => 1,
                    _ => 0,
                });
            if let Some(r) = best {
                sample_format = r.sample_format();
                config = r.with_sample_rate(cpal::SampleRate(target_rate)).into();
            }
        }
    }

    Ok(SelectedDevice {
        device,
        name,
        config,
        sample_format,
    })
}

pub fn select_output_device(
    requested_name: Option<&str>,
    preferred_rate: Option<u32>,
) -> Result<SelectedDevice, String> {
    let host = cpal::default_host();

    let device = if let Some(query) = requested_name {
        let q_lower = query.to_lowercase();
        let mut matched = None;
        if let Ok(devices) = host.output_devices() {
            for (idx, d) in devices.enumerate() {
                let name = d.name().unwrap_or_default();
                if idx.to_string() == query || name.to_lowercase().contains(&q_lower) {
                    matched = Some(d);
                    break;
                }
            }
        }
        matched.ok_or_else(|| {
            format!(
                "Output device matching '{}' not found. Run `lan-audio devices` to list options.",
                query
            )
        })?
    } else {
        let mut pulse_dev = None;
        if let Ok(devices) = host.output_devices() {
            for d in devices {
                if d.name().unwrap_or_default().eq_ignore_ascii_case("pulse") {
                    pulse_dev = Some(d);
                    break;
                }
            }
        }
        if let Some(d) = pulse_dev {
            d
        } else {
            host.default_output_device()
                .ok_or_else(|| "No default output audio device found".to_string())?
        }
    };

    let name = device.name().unwrap_or_else(|_| "default".into());
    let default_cfg = device
        .default_output_config()
        .map_err(|e| format!("Failed to query output config for '{}': {}", name, e))?;

    let mut sample_format = default_cfg.sample_format();
    let mut config: StreamConfig = default_cfg.into();

    if let Some(target_rate) = preferred_rate {
        if let Ok(ranges) = device.supported_output_configs() {
            let best = ranges
                .filter(|r| {
                    r.min_sample_rate().0 <= target_rate
                        && r.max_sample_rate().0 >= target_rate
                        && r.channels() == 2
                        && matches!(r.sample_format(), SampleFormat::F32 | SampleFormat::I16)
                })
                .max_by_key(|r| match r.sample_format() {
                    SampleFormat::F32 => 2,
                    SampleFormat::I16 => 1,
                    _ => 0,
                });
            if let Some(r) = best {
                sample_format = r.sample_format();
                config = r.with_sample_rate(cpal::SampleRate(target_rate)).into();
            }
        }
    }

    Ok(SelectedDevice {
        device,
        name,
        config,
        sample_format,
    })
}

/// Build a low-latency CPAL input stream (requests 128-frame = 2.6ms buffer on macOS CoreAudio)
pub fn build_stereo_input_stream<F>(
    selected: &SelectedDevice,
    on_frames: F,
) -> Result<Stream, String>
where
    F: FnMut(&[[f32; 2]]) + Send + 'static,
{
    let on_frames = std::sync::Arc::new(std::sync::Mutex::new(on_frames));
    for buf_size in [
        BufferSize::Fixed(128),
        BufferSize::Fixed(256),
        BufferSize::Default,
    ] {
        let mut cfg = selected.config.clone();
        cfg.buffer_size = buf_size;
        let cb = std::sync::Arc::clone(&on_frames);
        if let Ok(stream) = try_build_input_stream(&selected.device, &cfg, selected.sample_format, cb) {
            if stream.play().is_ok() {
                return Ok(stream);
            }
        }
    }
    Err("Failed to open input audio stream with any buffer configuration".into())
}

fn try_build_input_stream<F>(
    device: &Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    on_frames: std::sync::Arc<std::sync::Mutex<F>>,
) -> Result<Stream, cpal::BuildStreamError>
where
    F: FnMut(&[[f32; 2]]) + Send + 'static,
{
    let channels = config.channels as usize;
    let err_fn = |err| eprintln!("\n[!] Audio input stream error: {}", err);

    match sample_format {
        SampleFormat::F32 => {
            let mut stereo = Vec::with_capacity(512);
            device.build_input_stream(
                config,
                move |data: &[f32], _| {
                    stereo.clear();
                    for chunk in data.chunks_exact(channels.max(1)) {
                        let l = chunk[0];
                        let r = if channels >= 2 { chunk[1] } else { l };
                        stereo.push([l, r]);
                    }
                    if let Ok(mut cb) = on_frames.lock() {
                        cb(&stereo);
                    }
                },
                err_fn,
                None,
            )
        }
        SampleFormat::I16 => {
            let mut stereo = Vec::with_capacity(512);
            device.build_input_stream(
                config,
                move |data: &[i16], _| {
                    stereo.clear();
                    for chunk in data.chunks_exact(channels.max(1)) {
                        let l = chunk[0] as f32 / 32768.0;
                        let r = if channels >= 2 {
                            chunk[1] as f32 / 32768.0
                        } else {
                            l
                        };
                        stereo.push([l, r]);
                    }
                    if let Ok(mut cb) = on_frames.lock() {
                        cb(&stereo);
                    }
                },
                err_fn,
                None,
            )
        }
        SampleFormat::U16 => {
            let mut stereo = Vec::with_capacity(512);
            device.build_input_stream(
                config,
                move |data: &[u16], _| {
                    stereo.clear();
                    for chunk in data.chunks_exact(channels.max(1)) {
                        let l = (chunk[0] as f32 - 32768.0) / 32768.0;
                        let r = if channels >= 2 {
                            (chunk[1] as f32 - 32768.0) / 32768.0
                        } else {
                            l
                        };
                        stereo.push([l, r]);
                    }
                    if let Ok(mut cb) = on_frames.lock() {
                        cb(&stereo);
                    }
                },
                err_fn,
                None,
            )
        }
        _ => Err(cpal::BuildStreamError::StreamConfigNotSupported),
    }
}

/// Build a low-latency CPAL output stream (requests 128-frame = 2.6ms buffer first)
pub fn build_stereo_output_stream<F>(
    selected: &SelectedDevice,
    fill_frames: F,
) -> Result<Stream, String>
where
    F: FnMut(&mut [[f32; 2]]) + Send + 'static,
{
    let fill_frames = std::sync::Arc::new(std::sync::Mutex::new(fill_frames));
    for buf_size in [
        BufferSize::Fixed(128),
        BufferSize::Fixed(256),
        BufferSize::Fixed(512),
        BufferSize::Default,
    ] {
        let mut cfg = selected.config.clone();
        cfg.buffer_size = buf_size;
        let cb = std::sync::Arc::clone(&fill_frames);
        if let Ok(stream) =
            try_build_output_stream(&selected.device, &cfg, selected.sample_format, cb)
        {
            if stream.play().is_ok() {
                return Ok(stream);
            }
        }
    }
    Err("Failed to open output audio stream with any buffer configuration".into())
}

fn try_build_output_stream<F>(
    device: &Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    fill_frames: std::sync::Arc<std::sync::Mutex<F>>,
) -> Result<Stream, cpal::BuildStreamError>
where
    F: FnMut(&mut [[f32; 2]]) + Send + 'static,
{
    let channels = config.channels as usize;
    let err_fn = |err| eprintln!("\n[!] Audio output stream error: {}", err);

    match sample_format {
        SampleFormat::F32 => {
            let mut scratch: Vec<[f32; 2]> = Vec::with_capacity(1024);
            device.build_output_stream(
                config,
                move |data: &mut [f32], _| {
                    let frames = data.len() / channels.max(1);
                    scratch.resize(frames, [0.0, 0.0]);
                    if let Ok(mut cb) = fill_frames.lock() {
                        cb(&mut scratch);
                    }
                    for (i, chunk) in data.chunks_exact_mut(channels.max(1)).enumerate() {
                        let [l, r] = scratch[i];
                        chunk[0] = l;
                        if channels >= 2 {
                            chunk[1] = r;
                            for c in &mut chunk[2..] {
                                *c = 0.0;
                            }
                        }
                    }
                },
                err_fn,
                None,
            )
        }
        SampleFormat::I16 => {
            let mut scratch: Vec<[f32; 2]> = Vec::with_capacity(1024);
            device.build_output_stream(
                config,
                move |data: &mut [i16], _| {
                    let frames = data.len() / channels.max(1);
                    scratch.resize(frames, [0.0, 0.0]);
                    if let Ok(mut cb) = fill_frames.lock() {
                        cb(&mut scratch);
                    }
                    for (i, chunk) in data.chunks_exact_mut(channels.max(1)).enumerate() {
                        let [l, r] = scratch[i];
                        chunk[0] = (l.clamp(-1.0, 1.0) * 32767.0) as i16;
                        if channels >= 2 {
                            chunk[1] = (r.clamp(-1.0, 1.0) * 32767.0) as i16;
                            for c in &mut chunk[2..] {
                                *c = 0;
                            }
                        }
                    }
                },
                err_fn,
                None,
            )
        }
        SampleFormat::U16 => {
            let mut scratch: Vec<[f32; 2]> = Vec::with_capacity(1024);
            device.build_output_stream(
                config,
                move |data: &mut [u16], _| {
                    let frames = data.len() / channels.max(1);
                    scratch.resize(frames, [0.0, 0.0]);
                    if let Ok(mut cb) = fill_frames.lock() {
                        cb(&mut scratch);
                    }
                    for (i, chunk) in data.chunks_exact_mut(channels.max(1)).enumerate() {
                        let [l, r] = scratch[i];
                        chunk[0] = ((l.clamp(-1.0, 1.0) * 32767.0) + 32768.0) as u16;
                        if channels >= 2 {
                            chunk[1] = ((r.clamp(-1.0, 1.0) * 32767.0) + 32768.0) as u16;
                            for c in &mut chunk[2..] {
                                *c = 32768;
                            }
                        }
                    }
                },
                err_fn,
                None,
            )
        }
        _ => Err(cpal::BuildStreamError::StreamConfigNotSupported),
    }
}
