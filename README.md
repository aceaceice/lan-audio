# lan-audio

Ultra-low-latency LAN audio streamer in Rust (`cpal` + UDP) for streaming sound from a **macOS laptop** to **Linux PCs** (or between any mix of Linux and macOS machines) with **automatic LAN discovery**.

## Features
- **Zero-config LAN Discovery**: Receivers announce themselves via UDP broadcast & multicast (`54320`). Senders automatically find all receivers on your LAN without typing IP addresses.
- **Multi-Device / 1-to-Many Streaming**: Stream audio from your laptop to one or multiple Linux PCs simultaneously.
- **Adaptive Jitter Buffer & Fractional Resampler**: Automatically converts between macOS (`48000 Hz`) and Linux hardware sample rates (`44100 Hz` / `48000 Hz`) with continuous clock-drift compensation so latency stays locked (~25 ms) over hours of playback.
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
2. Build and run the sender on your Mac:
   ```bash
   cargo build --release
   ./target/release/lan-audio send
   ```
   `lan-audio send` will automatically discover your Linux PC on the LAN and begin streaming immediately!

---

## CLI Reference

```bash
# List all input, output, and loopback audio devices
lan-audio devices

# Scan the local network for active receivers
lan-audio discover

# Start receiver (custom latency or volume)
lan-audio recv --latency-ms 25 --volume 1.0

# Start sender (auto-discover receivers on LAN)
lan-audio send

# Start sender targeting a specific IP (or multiple IPs)
lan-audio send --target 192.168.1.50

# Use 4-bit ADPCM compression for congested Wi-Fi
lan-audio send --codec adpcm
```
