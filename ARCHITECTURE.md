# `lan-audio` Architecture & Context

This document captures the technical design, wire protocol, latency pipeline, and future roadmap for `lan-audio`.

---

## 1. Overview & Pipeline

`lan-audio` streams low-latency stereo audio over LAN between macOS and Linux devices without manual IP configuration.

```
[Sender (macOS / Linux)]                                [Receiver (Linux / macOS)]
┌────────────────────────────┐                          ┌──────────────────────────────┐
│ Audio Capture Thread       │                          │ UDP Receive Thread (:54321)  │
│ • macOS: CoreAudio 128f    │                          │ • Decodes LANA packet        │
│   (BlackHole 2ch loopback) │                          │ • Pushes to JitterBuffer     │
│ • Linux: PulseAudio direct │                          └──────────────┬───────────────┘
│   (.monitor source, 120f)  │                                         │
└─────────────┬──────────────┘                                         ▼
              │ Bounded MPSC (64)                       ┌──────────────────────────────┐
              ▼                                         │ Adaptive JitterBuffer        │
┌────────────────────────────┐      UDP (2.5ms/pkt)     │ • Target: 20ms (Cap: 45ms)   │
│ UDP Sender & DTX Thread    │ ───────────────────────► │ • Two-stage fractional       │
│ • 120 frames/pkt (2.5ms)   │    :54321 (Audio Data)   │   resampler (clock drift)    │
│ • Smart Silence Pause      │    :54320 (Discovery)    │ • Sunshine-style low-buffer  │
│   (500ms tail, 0ms wake)   │                          │   PLC & 16-sample crossfade  │
└────────────────────────────┘                          └──────────────┬───────────────┘
                                                                       │
                                                                       ▼
                                                        ┌──────────────────────────────┐
                                                        │ Audio Playback Thread        │
                                                        │ • Linux: libpulse-simple FFI │
                                                        │   (10ms server buf, 120f)    │
                                                        │ • macOS/Fallback: CPAL 128f  │
                                                        └──────────────────────────────┘
```

---

## 2. Binary Wire Protocols (`src/protocol.rs`)

### Discovery Protocol (`UDP :54320`, Multicast `239.255.77.88` + Broadcast)
- Magic: `"LANAUD01"` (8 bytes)
- **Type `1` (`Query`)**: Sent by `lan-audio send` or `lan-audio discover`. Receivers immediately reply via unicast to the sender's source `IP:port`.
- **Type `2` (`ReceiverAnnounce`)**: Sent by `lan-audio recv` every `1.5s` and on `Query`. Includes `audio_port (u16)`, `sample_rate (u32)`, `channels (u8)`, and `hostname`.

### Audio Data Protocol (`UDP :54321`)
Every UDP packet has a **24-byte header** (`AudioHeader`) followed by `120` stereo frames (`2.5 ms` @ 48 kHz = `480 bytes` in `pcm-i16`, well below Wi-Fi MTU to prevent IP fragmentation):

| Offset | Size | Field | Description |
| :--- | :--- | :--- | :--- |
| `0..4` | 4B | `magic` | `"LANA"` |
| `4` | 1B | `version` | `1` |
| `5` | 1B | `codec` | `0 = PcmI16`, `1 = PcmF32`, `2 = Adpcm` |
| `6` | 1B | `channels` | `2` (stereo) |
| `7` | 1B | `flags` | Bit `0x01` = `FLAG_STREAM_RESUMED` (waking from DTX silence pause) |
| `8..12` | 4B | `seq` | Monotonic packet sequence number (`u32` LE) |
| `12..16` | 4B | `sample_rate` | Capture sample rate in Hz (`u32` LE, typically `48000`) |
| `16..18` | 2B | `frame_count` | Number of stereo frames in payload (`u16` LE, default `120`) |
| `18..24` | 6B | `adpcm_state` | Independent per-packet IMA ADPCM predictor (`i16`) + step index (`u8`) for L/R |

---

## 3. Jitter Buffer, PLC & DTX (`src/dsp.rs`, `src/main.rs`)

1. **Dynamic Buffer Cap (`--latency-ms` & `--max-latency-ms`)**:
   - Rather than hardcoding a fixed ceiling, `max_latency_ms` defaults to `max(target * 2.25, target + 20ms)` (`45 ms` for the default `20 ms` target) and can be overridden via `--max-latency-ms`.
   - If a Wi-Fi stall dumps a large burst of delayed packets that exceeds `max_latency_ms`, the stale excess is immediately trimmed back to `target_latency_ms` with a `32`-sample crossfade so latency never balloons.
2. **Sunshine/WebRTC-Style Packet Loss Concealment (PLC)**:
   - Synthetic interpolation frames are **only** inserted when `1..=4` packets are missing **and** the buffer is below `50%` of target (about to underrun).
   - If the buffer already has healthy margin (`>= 50%`), no synthetic frames are added (preventing buffer ratchet/inflation), and a `16`-sample boundary crossfade prevents any waveform click.
3. **Two-Stage Fractional Resampler**:
   - Compensates for hardware crystal oscillator drift between the sender and receiver.
   - Within `±25%` of target buffer fill: gentle `±0.35%` speed adjustment (inaudible).
   - Above `1.25x` target buffer fill: ramps up to `+1.25%` catch-up so minor Wi-Fi bursts settle back to `20 ms` within ~1 second.
4. **Smart Silence Suppression / DTX (`run_sender`)**:
   - When the sender captures digital silence (`peak <= -82 dBFS`) continuously for `500 ms` (`200` packets), it fades the final packet to `0.0` and pauses UDP transmission (`0.0 kbps`).
   - The capture thread continues inspecting every `2.5 ms` chunk; the instant audio resumes, it sets `FLAG_STREAM_RESUMED` and transmits with `0 ms` wake-up delay.

---

## 4. Gruvbox GUI, Ricing Engine & macOS Auto-Routing (`src/gui.rs`, `src/theme.rs`, `src/audio.rs`)

1. **Waybar / GTK `@define-color` Ricing Engine (`src/theme.rs`)**:
   - Auto-generates `~/.config/lan-audio/style.css` with the Gruvbox palette (`@black`, `@bg2`, `@braqua`, `@brpurple`, `@bryellow`, etc.) and semantic roles (`@sound`, `@network`, `@focused`, `@unfocused`, `@warning`, `@critical`, `@memory`, `@temp`, `@layout`, `@date`, `@time`) on first launch.
   - Polls `mtime` every `500 ms` and **hot-reloads live** whenever `~/.config/lan-audio/style.css` is saved (compatible with dotfile theme switchers, `pywal`, `matugen`, `wallust`, or manual edits).
   - Also supports geometry variables (`border-radius`, `border-width`, `font-scale`).
2. **macOS CoreAudio Automatic Output Switcher (`MacOutputGuard` in `src/audio.rs`)**:
   - Uses `AudioObjectGetPropertyData` / `AudioObjectSetPropertyData` (`kAudioHardwarePropertyDefaultOutputDevice`) directly via `CoreAudio.framework` FFI—requiring **zero macOS TCC Screen/Audio Recording permissions**.
   - Supports 4 modes (`Auto`, `MultiOutput` [PC + Mac], `PcOnly` [BlackHole, muting Mac speakers], `Manual`).
   - Saves the original output device ID (e.g., *MacBook Pro Speakers*) and automatically restores it when clicking **STOP**, closing the GUI window, or pressing **`Ctrl+C`** in CLI mode.
3. **Standalone macOS `.app` Bundle (`scripts/bundle-macos-app.sh`)**:
   - Packages the release binary into `target/release/LAN Audio.app` so it can be launched from `/Applications`, Spotlight, or the Dock without opening Terminal.

---

## 5. Roadmap / Future Convenience Features

- **Native macOS `ScreenCaptureKit` Capture**: Capture macOS desktop system audio directly on macOS 13+ without requiring `BlackHole 2ch` or Audio MIDI Setup.
- **Interactive TUI / System Tray App**: Toggle receivers, adjust per-device volume, mute/unmute, and switch audio devices live.
- **Systemd / LaunchAgent Background Service**: `lan-audio service install` to auto-start `recv` on Linux boot and `send` on macOS login.
- **Opus Ultra-Low-Latency Codec Option**: Optional `opus` (`2.5ms`/`5ms` frames) for lower bandwidth over congested Wi-Fi.
