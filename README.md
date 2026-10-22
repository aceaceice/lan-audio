# lan-audio

Ultra-low-latency LAN audio streamer in Rust (`cpal` + UDP) for streaming sound from a **macOS laptop** to **Linux PCs** (or between any mix of Linux and macOS machines) with **automatic LAN discovery**.

## Features
- **Minimalist Gruvbox GUI + Ricing Engine (`~/.config/lan-audio/style.css`)**:
  - Launches into a Waybar/tiling-WM-inspired Gruvbox GUI (`lan-audio` or `LAN Audio.app`).
  - Supports **Waybar / GTK `@define-color` CSS syntax** in `~/.config/lan-audio/style.css` with **live hot-reloading** while the app is open.
- **Automatic macOS Output Switching & Restoration**:
  - Automatically switches your Mac's audio output to **Multi-Output Device** (PC + Mac) or **BlackHole 2ch** (PC only, muting Mac speakers) when streaming starts, and **automatically restores MacBook Speakers** when you stop or press `Ctrl+C`—requiring zero special macOS TCC permissions.
- **Zero-config LAN Discovery**: Receivers announce themselves via UDP broadcast & multicast (`54320`). Senders automatically find all receivers on your LAN without typing IP addresses.
- **Multi-Device / 1-to-Many Streaming**: Stream audio from your laptop to one or multiple Linux PCs simultaneously.
- **Adaptive Jitter Buffer & Fractional Resampler**: Automatically converts between macOS (`48000 Hz`) and Linux hardware sample rates (`44100 Hz` / `48000 Hz`) with continuous clock-drift compensation and configurable burst cap so latency stays locked (~20 ms).
- **Smart Silence Pause (DTX)**: Automatically pauses UDP transmission (`0.0 kbps`) after `500 ms` when no sound is playing on your Mac, and resumes in `0 ms` the instant sound plays again.
- **Smart Loopback Selection**:
  - **macOS**: Automatically selects virtual loopback drivers (`BlackHole 2ch`, `Loopback`, `Soundflower`) when sending desktop audio.
  - **Linux**: Automatically captures desktop audio from the default PulseAudio/PipeWire `.monitor` sink (or microphone with `--mic`).
- **Three Codecs**:
  - `pcm-i16` (default): Uncompressed 16-bit stereo PCM (~1.5 Mbps at 48 kHz)
  - `adpcm`: IMA ADPCM 4-bit compressed (~384 kbps, resilient to packet loss—each UDP packet carries independent predictor state)
  - `pcm-f32`: Uncompressed 32-bit float stereo PCM

---

## Quick Start

### 1. On your Linux PC (Receiver)
Build and start the receiver:
```bash
cargo build --release
./target/release/lan-audio recv
```

### 2. On your macOS Laptop (Sender)
1. Install [BlackHole](https://existential.audio/blackhole/) so macOS can capture system desktop audio:
   ```bash
   brew install blackhole-2ch
   ```
   *(Set your macOS Sound Output to **BlackHole 2ch**, or create a **Multi-Output Device** in macOS `Audio MIDI Setup.app` if you want to hear sound on both your Mac speakers and your Linux PC).*
2. Build and run the GUI or CLI sender on your Mac:
   ```bash
   # Option A: Build a standalone macOS "LAN Audio.app" bundle for /Applications or Spotlight
   ./scripts/bundle-macos-app.sh
   cp -R "target/release/LAN Audio.app" /Applications/

   # Option B: Run directly from terminal (auto-switches Mac output & restores on Ctrl+C)
   cargo build --release
   ./target/release/lan-audio        # Opens Gruvbox GUI
   ./target/release/lan-audio send   # CLI sender (auto-routes Mac audio)
   ```
   `lan-audio` will automatically switch your Mac output to your Multi-Output/BlackHole device, discover your Linux PC on the LAN, and restore your Mac speakers as soon as you stop!

---

## CLI Reference

```bash
# Launch the minimalist Gruvbox GUI (default when run with no arguments)
lan-audio
lan-audio gui --style ~/.config/lan-audio/style.css

# List all input, output, and loopback audio devices
lan-audio devices

# Scan the local network for active receivers
lan-audio discover

# Start receiver (custom target latency, max burst cap, or volume)
lan-audio recv --latency-ms 20 --max-latency-ms 45 --volume 1.0

# Start sender (auto-switches Mac output to Multi-Output/BlackHole and restores on Ctrl+C)
lan-audio send

# Start sender playing on PC ONLY (switches Mac output to BlackHole 2ch to mute Mac speakers)
lan-audio send --route pc-only

# Start sender with continuous streaming even during silence
lan-audio send --no-silence-pause

# Start sender targeting a specific IP (or multiple IPs)
lan-audio send --target 192.168.1.50

# Use 4-bit ADPCM compression for congested Wi-Fi
lan-audio send --codec adpcm
```
