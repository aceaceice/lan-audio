use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig};
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
            // Prefer 2ch F32 or I16 at target_rate; otherwise keep default_input_config()
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
        // On Linux with PulseAudio/PipeWire, prefer the "pulse" device so audio routes
        // to the user's active PulseAudio default sink (e.g. USB audio interface/headphones)
        // even if `pulseaudio-alsa` is not installed.
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

/// Build a CPAL input stream that converts any hardware channel/sample layout into stereo `[f32; 2]` frames
pub fn build_stereo_input_stream<F>(
    selected: &SelectedDevice,
    mut on_frames: F,
) -> Result<Stream, String>
where
    F: FnMut(&[[f32; 2]]) + Send + 'static,
{
    let channels = selected.config.channels as usize;
    let err_fn = |err| eprintln!("\n[!] Audio input stream error: {}", err);

    let stream = match selected.sample_format {
        SampleFormat::F32 => selected
            .device
            .build_input_stream(
                &selected.config,
                move |data: &[f32], _| {
                    let mut stereo = Vec::with_capacity(data.len() / channels.max(1));
                    for chunk in data.chunks_exact(channels.max(1)) {
                        let l = chunk[0];
                        let r = if channels >= 2 { chunk[1] } else { l };
                        stereo.push([l, r]);
                    }
                    on_frames(&stereo);
                },
                err_fn,
                None,
            )
            .map_err(|e| e.to_string())?,
        SampleFormat::I16 => selected
            .device
            .build_input_stream(
                &selected.config,
                move |data: &[i16], _| {
                    let mut stereo = Vec::with_capacity(data.len() / channels.max(1));
                    for chunk in data.chunks_exact(channels.max(1)) {
                        let l = chunk[0] as f32 / 32768.0;
                        let r = if channels >= 2 {
                            chunk[1] as f32 / 32768.0
                        } else {
                            l
                        };
                        stereo.push([l, r]);
                    }
                    on_frames(&stereo);
                },
                err_fn,
                None,
            )
            .map_err(|e| e.to_string())?,
        SampleFormat::U16 => selected
            .device
            .build_input_stream(
                &selected.config,
                move |data: &[u16], _| {
                    let mut stereo = Vec::with_capacity(data.len() / channels.max(1));
                    for chunk in data.chunks_exact(channels.max(1)) {
                        let l = (chunk[0] as f32 - 32768.0) / 32768.0;
                        let r = if channels >= 2 {
                            (chunk[1] as f32 - 32768.0) / 32768.0
                        } else {
                            l
                        };
                        stereo.push([l, r]);
                    }
                    on_frames(&stereo);
                },
                err_fn,
                None,
            )
            .map_err(|e| e.to_string())?,
        other => return Err(format!("Unsupported input sample format: {:?}", other)),
    };

    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

/// Build a CPAL output stream that pulls stereo `[f32; 2]` frames and writes to any hardware channel/sample layout
pub fn build_stereo_output_stream<F>(
    selected: &SelectedDevice,
    mut fill_frames: F,
) -> Result<Stream, String>
where
    F: FnMut(&mut [[f32; 2]]) + Send + 'static,
{
    let channels = selected.config.channels as usize;
    let err_fn = |err| eprintln!("\n[!] Audio output stream error: {}", err);

    let stream = match selected.sample_format {
        SampleFormat::F32 => {
            let mut scratch: Vec<[f32; 2]> = Vec::with_capacity(2048);
            selected
                .device
                .build_output_stream(
                    &selected.config,
                    move |data: &mut [f32], _| {
                        let frames = data.len() / channels.max(1);
                        scratch.resize(frames, [0.0, 0.0]);
                        fill_frames(&mut scratch);
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
                .map_err(|e| e.to_string())?
        }
        SampleFormat::I16 => {
            let mut scratch: Vec<[f32; 2]> = Vec::with_capacity(2048);
            selected
                .device
                .build_output_stream(
                    &selected.config,
                    move |data: &mut [i16], _| {
                        let frames = data.len() / channels.max(1);
                        scratch.resize(frames, [0.0, 0.0]);
                        fill_frames(&mut scratch);
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
                .map_err(|e| e.to_string())?
        }
        SampleFormat::U16 => {
            let mut scratch: Vec<[f32; 2]> = Vec::with_capacity(2048);
            selected
                .device
                .build_output_stream(
                    &selected.config,
                    move |data: &mut [u16], _| {
                        let frames = data.len() / channels.max(1);
                        scratch.resize(frames, [0.0, 0.0]);
                        fill_frames(&mut scratch);
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
                .map_err(|e| e.to_string())?
        }
        other => return Err(format!("Unsupported output sample format: {:?}", other)),
    };

    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}
