use crate::protocol::{AudioHeader, Codec, PROTOCOL_VERSION};
use std::collections::VecDeque;

const IMA_INDEX_TABLE: [i32; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

const IMA_STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

#[derive(Default, Clone, Copy)]
pub struct AdpcmChannelState {
    pub predictor: i16,
    pub step_index: u8,
}

impl AdpcmChannelState {
    pub fn encode_sample(&mut self, sample: i16) -> u8 {
        let step = IMA_STEP_TABLE[self.step_index as usize];
        let mut diff = (sample as i32) - (self.predictor as i32);
        let mut nibble = 0u8;
        if diff < 0 {
            nibble = 8;
            diff = -diff;
        }
        let mut vpdiff = step >> 3;
        if diff >= step {
            nibble |= 4;
            diff -= step;
            vpdiff += step;
        }
        if diff >= (step >> 1) {
            nibble |= 2;
            diff -= step >> 1;
            vpdiff += step >> 1;
        }
        if diff >= (step >> 2) {
            nibble |= 1;
            vpdiff += step >> 2;
        }

        let mut pred = self.predictor as i32;
        if (nibble & 8) != 0 {
            pred -= vpdiff;
        } else {
            pred += vpdiff;
        }
        self.predictor = pred.clamp(-32768, 32767) as i16;

        let idx = (self.step_index as i32 + IMA_INDEX_TABLE[(nibble & 0x0F) as usize]).clamp(0, 88);
        self.step_index = idx as u8;
        nibble & 0x0F
    }

    pub fn decode_nibble(&mut self, nibble: u8) -> i16 {
        let step = IMA_STEP_TABLE[self.step_index.min(88) as usize];
        let mut vpdiff = step >> 3;
        if (nibble & 4) != 0 {
            vpdiff += step;
        }
        if (nibble & 2) != 0 {
            vpdiff += step >> 1;
        }
        if (nibble & 1) != 0 {
            vpdiff += step >> 2;
        }
        let mut pred = self.predictor as i32;
        if (nibble & 8) != 0 {
            pred -= vpdiff;
        } else {
            pred += vpdiff;
        }
        self.predictor = pred.clamp(-32768, 32767) as i16;
        let idx = (self.step_index as i32 + IMA_INDEX_TABLE[(nibble & 0x0F) as usize]).clamp(0, 88);
        self.step_index = idx as u8;
        self.predictor
    }
}

pub fn encode_audio_packet(
    codec: Codec,
    seq: u32,
    sample_rate: u32,
    stereo_frames: &[[f32; 2]],
    adpcm_states: &mut [AdpcmChannelState; 2],
    out_buf: &mut Vec<u8>,
) {
    out_buf.clear();
    let frame_count = stereo_frames.len() as u16;
    let mut adpcm_hdr = [0u8; 6];
    if codec == Codec::Adpcm {
        adpcm_hdr[0..2].copy_from_slice(&adpcm_states[0].predictor.to_le_bytes());
        adpcm_hdr[2] = adpcm_states[0].step_index;
        adpcm_hdr[3..5].copy_from_slice(&adpcm_states[1].predictor.to_le_bytes());
        adpcm_hdr[5] = adpcm_states[1].step_index;
    }

    let hdr = AudioHeader {
        version: PROTOCOL_VERSION,
        codec,
        channels: 2,
        flags: 0,
        seq,
        sample_rate,
        frame_count,
        adpcm_state: adpcm_hdr,
    };

    out_buf.resize(AudioHeader::SIZE, 0);
    hdr.encode(&mut out_buf[..AudioHeader::SIZE]);

    match codec {
        Codec::PcmI16 => {
            out_buf.reserve(stereo_frames.len() * 4);
            for &[l, r] in stereo_frames {
                let li = (l.clamp(-1.0, 1.0) * 32767.0) as i16;
                let ri = (r.clamp(-1.0, 1.0) * 32767.0) as i16;
                out_buf.extend_from_slice(&li.to_le_bytes());
                out_buf.extend_from_slice(&ri.to_le_bytes());
            }
        }
        Codec::PcmF32 => {
            out_buf.reserve(stereo_frames.len() * 8);
            for &[l, r] in stereo_frames {
                out_buf.extend_from_slice(&l.to_le_bytes());
                out_buf.extend_from_slice(&r.to_le_bytes());
            }
        }
        Codec::Adpcm => {
            out_buf.reserve(stereo_frames.len());
            for &[l, r] in stereo_frames {
                let li = (l.clamp(-1.0, 1.0) * 32767.0) as i16;
                let ri = (r.clamp(-1.0, 1.0) * 32767.0) as i16;
                let nl = adpcm_states[0].encode_sample(li);
                let nr = adpcm_states[1].encode_sample(ri);
                out_buf.push((nl & 0x0F) | ((nr & 0x0F) << 4));
            }
        }
    }
}

pub fn decode_audio_packet(packet: &[u8], out_frames: &mut Vec<[f32; 2]>) -> Option<AudioHeader> {
    let hdr = AudioHeader::decode(packet)?;
    let payload = &packet[AudioHeader::SIZE..];
    let frames = hdr.frame_count as usize;
    out_frames.clear();
    out_frames.reserve(frames);

    match hdr.codec {
        Codec::PcmI16 => {
            let ch = hdr.channels as usize;
            let bytes_per_frame = ch * 2;
            if payload.len() < frames * bytes_per_frame {
                return None;
            }
            for i in 0..frames {
                let base = i * bytes_per_frame;
                let l = i16::from_le_bytes([payload[base], payload[base + 1]]) as f32 / 32768.0;
                let r = if ch >= 2 {
                    i16::from_le_bytes([payload[base + 2], payload[base + 3]]) as f32 / 32768.0
                } else {
                    l
                };
                out_frames.push([l, r]);
            }
        }
        Codec::PcmF32 => {
            let ch = hdr.channels as usize;
            let bytes_per_frame = ch * 4;
            if payload.len() < frames * bytes_per_frame {
                return None;
            }
            for i in 0..frames {
                let base = i * bytes_per_frame;
                let l = f32::from_le_bytes([
                    payload[base],
                    payload[base + 1],
                    payload[base + 2],
                    payload[base + 3],
                ]);
                let r = if ch >= 2 {
                    f32::from_le_bytes([
                        payload[base + 4],
                        payload[base + 5],
                        payload[base + 6],
                        payload[base + 7],
                    ])
                } else {
                    l
                };
                out_frames.push([l, r]);
            }
        }
        Codec::Adpcm => {
            if payload.len() < frames {
                return None;
            }
            let mut st_l = AdpcmChannelState {
                predictor: i16::from_le_bytes([hdr.adpcm_state[0], hdr.adpcm_state[1]]),
                step_index: hdr.adpcm_state[2].min(88),
            };
            let mut st_r = AdpcmChannelState {
                predictor: i16::from_le_bytes([hdr.adpcm_state[3], hdr.adpcm_state[4]]),
                step_index: hdr.adpcm_state[5].min(88),
            };
            for &byte in &payload[..frames] {
                let nl = byte & 0x0F;
                let nr = (byte >> 4) & 0x0F;
                let l = st_l.decode_nibble(nl) as f32 / 32768.0;
                let r = st_r.decode_nibble(nr) as f32 / 32768.0;
                out_frames.push([l, r]);
            }
        }
    }

    Some(hdr)
}

/// Adaptive jitter buffer and fractional resampler for smooth low-latency LAN playback
pub struct JitterBuffer {
    queue: VecDeque<[f32; 2]>,
    sender_sample_rate: u32,
    output_sample_rate: u32,
    target_latency_ms: f32,
    smoothed_fill_frames: f32,
    resample_phase: f64,
    last_frame: [f32; 2],
    primed: bool,
    last_seq: Option<u32>,
    pub packets_received: u64,
    pub packets_lost: u64,
    pub underruns: u64,
}

impl JitterBuffer {
    pub fn new(output_sample_rate: u32, target_latency_ms: f32) -> Self {
        let cap = ((output_sample_rate as usize) * 2).max(8192);
        Self {
            queue: VecDeque::with_capacity(cap),
            sender_sample_rate: output_sample_rate,
            output_sample_rate,
            target_latency_ms: target_latency_ms.clamp(5.0, 500.0),
            smoothed_fill_frames: 0.0,
            resample_phase: 0.0,
            last_frame: [0.0, 0.0],
            primed: false,
            last_seq: None,
            packets_received: 0,
            packets_lost: 0,
            underruns: 0,
        }
    }

    pub fn sender_sample_rate(&self) -> u32 {
        self.sender_sample_rate
    }

    pub fn current_latency_ms(&self) -> f32 {
        if self.sender_sample_rate == 0 {
            return 0.0;
        }
        (self.queue.len() as f32 * 1000.0) / (self.sender_sample_rate as f32)
    }

    pub fn push_packet(&mut self, hdr: &AudioHeader, frames: &[[f32; 2]]) {
        if hdr.sample_rate >= 8000 && hdr.sample_rate <= 192_000 {
            self.sender_sample_rate = hdr.sample_rate;
        }
        self.packets_received += 1;

        if let Some(prev) = self.last_seq {
            let expected = prev.wrapping_add(1);
            if hdr.seq != expected {
                let diff = hdr.seq.wrapping_sub(expected);
                if diff < 1000 {
                    self.packets_lost += diff as u64;
                }
            }
        }
        self.last_seq = Some(hdr.seq);

        let target_frames =
            ((self.target_latency_ms * 0.001) * (self.sender_sample_rate as f32)).max(240.0);
        let max_frames = (target_frames * 3.0).max(target_frames + 1440.0) as usize;

        for &frame in frames {
            self.queue.push_back(frame);
        }

        // Hard bound if burst/stall accumulated too many frames
        if self.queue.len() > max_frames {
            let drop_count = self.queue.len() - (target_frames as usize);
            self.queue.drain(..drop_count);
            self.smoothed_fill_frames = target_frames;
        }

        if !self.primed && (self.queue.len() as f32) >= target_frames * 0.75 {
            self.primed = true;
            self.smoothed_fill_frames = self.queue.len() as f32;
        }
    }

    /// Pull `out_frames.len()` stereo frames at `self.output_sample_rate` with volume scaling
    pub fn pop_output_frames(&mut self, out_frames: &mut [[f32; 2]], volume: f32) {
        if !self.primed || self.queue.len() < 2 {
            if self.primed {
                self.primed = false;
                self.underruns += 1;
            }
            // Smoothly decay last sample to zero to avoid audible click
            for frame in out_frames.iter_mut() {
                self.last_frame[0] *= 0.92;
                self.last_frame[1] *= 0.92;
                *frame = [self.last_frame[0] * volume, self.last_frame[1] * volume];
            }
            return;
        }

        let target_frames =
            ((self.target_latency_ms * 0.001) * (self.sender_sample_rate as f32)).max(240.0);
        let current_fill = self.queue.len() as f32;
        self.smoothed_fill_frames = 0.98 * self.smoothed_fill_frames + 0.02 * current_fill;

        // Gentle proportional drift control (clamped to +/- 0.25% pitch shift)
        let fill_error = (self.smoothed_fill_frames - target_frames) / target_frames.max(1.0);
        let drift_factor = (1.0 + (fill_error * 0.0015).clamp(-0.0025, 0.0025)) as f64;
        let base_ratio = (self.sender_sample_rate as f64) / (self.output_sample_rate.max(1) as f64);
        let step = base_ratio * drift_factor;

        for out in out_frames.iter_mut() {
            while self.resample_phase >= 1.0 {
                self.resample_phase -= 1.0;
                if let Some(f) = self.queue.pop_front() {
                    self.last_frame = f;
                } else {
                    self.primed = false;
                    self.underruns += 1;
                    break;
                }
            }

            if self.queue.len() >= 2 {
                let f0 = self.queue[0];
                let f1 = self.queue[1];
                let t = self.resample_phase as f32;
                let l = f0[0] + (f1[0] - f0[0]) * t;
                let r = f0[1] + (f1[1] - f0[1]) * t;
                self.last_frame = [l, r];
                *out = [
                    (l * volume).clamp(-1.0, 1.0),
                    (r * volume).clamp(-1.0, 1.0),
                ];
                self.resample_phase += step;
            } else {
                self.last_frame[0] *= 0.95;
                self.last_frame[1] *= 0.95;
                *out = [self.last_frame[0] * volume, self.last_frame[1] * volume];
            }
        }
    }
}

pub fn compute_peak_and_rms(frames: &[[f32; 2]]) -> (f32, f32) {
    if frames.is_empty() {
        return (0.0, 0.0);
    }
    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f32;
    for &[l, r] in frames {
        let m = l.abs().max(r.abs());
        if m > peak {
            peak = m;
        }
        sum_sq += 0.5 * (l * l + r * r);
    }
    let rms = (sum_sq / frames.len() as f32).sqrt();
    (peak, rms)
}

pub fn format_vu_bar(peak: f32, width: usize) -> String {
    let db = if peak <= 0.0001 {
        -60.0
    } else {
        20.0 * peak.log10()
    };
    let norm = ((db + 50.0) / 50.0).clamp(0.0, 1.0);
    let filled = (norm * width as f32).round() as usize;
    let mut bar = String::with_capacity(width + 2);
    bar.push('[');
    for i in 0..width {
        if i < filled {
            bar.push('#');
        } else {
            bar.push('-');
        }
    }
    bar.push(']');
    bar
}
