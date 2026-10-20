use std::fmt;

pub const DEFAULT_DISCOVERY_PORT: u16 = 54320;
pub const DEFAULT_AUDIO_PORT: u16 = 54321;
pub const MULTICAST_ADDR: [u8; 4] = [239, 255, 77, 88];

pub const AUDIO_MAGIC: [u8; 4] = *b"LANA";
pub const DISCOVERY_MAGIC: [u8; 8] = *b"LANAUD01";
pub const PROTOCOL_VERSION: u8 = 1;

/// Default frames per UDP packet (120 frames = 2.5ms @ 48kHz, 480 bytes stereo i16 PCM)
pub const DEFAULT_PACKET_FRAMES: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[repr(u8)]
pub enum Codec {
    /// Uncompressed 16-bit signed integer PCM (low CPU, CD/studio quality, ~1.5 Mbps @ 48kHz stereo)
    PcmI16 = 0,
    /// Uncompressed 32-bit float PCM (~3.0 Mbps @ 48kHz stereo)
    PcmF32 = 1,
    /// IMA ADPCM 4-bit compressed PCM (50% bandwidth of i16, ~384 kbps @ 48kHz stereo)
    Adpcm = 2,
}

impl Codec {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(Self::PcmI16),
            1 => Some(Self::PcmF32),
            2 => Some(Self::Adpcm),
            _ => None,
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PcmI16 => write!(f, "pcm-i16"),
            Self::PcmF32 => write!(f, "pcm-f32"),
            Self::Adpcm => write!(f, "adpcm"),
        }
    }
}

/// 24-byte header preceding audio payload in every UDP packet
#[derive(Debug, Clone, Copy)]
pub struct AudioHeader {
    pub version: u8,
    pub codec: Codec,
    pub channels: u8,
    pub flags: u8,
    pub seq: u32,
    pub sample_rate: u32,
    pub frame_count: u16,
    pub adpcm_state: [u8; 6],
}

impl AudioHeader {
    pub const SIZE: usize = 24;

    pub fn encode(&self, out: &mut [u8]) {
        assert!(out.len() >= Self::SIZE);
        out[0..4].copy_from_slice(&AUDIO_MAGIC);
        out[4] = self.version;
        out[5] = self.codec as u8;
        out[6] = self.channels;
        out[7] = self.flags;
        out[8..12].copy_from_slice(&self.seq.to_le_bytes());
        out[12..16].copy_from_slice(&self.sample_rate.to_le_bytes());
        out[16..18].copy_from_slice(&self.frame_count.to_le_bytes());
        out[18..24].copy_from_slice(&self.adpcm_state);
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE || buf[0..4] != AUDIO_MAGIC {
            return None;
        }
        let version = buf[4];
        if version != PROTOCOL_VERSION {
            return None;
        }
        let codec = Codec::from_u8(buf[5])?;
        let channels = buf[6];
        if channels == 0 || channels > 8 {
            return None;
        }
        let flags = buf[7];
        let seq = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let sample_rate = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        let frame_count = u16::from_le_bytes([buf[16], buf[17]]);
        let mut adpcm_state = [0u8; 6];
        adpcm_state.copy_from_slice(&buf[18..24]);

        Some(Self {
            version,
            codec,
            channels,
            flags,
            seq,
            sample_rate,
            frame_count,
            adpcm_state,
        })
    }
}

#[derive(Debug, Clone)]
pub enum DiscoveryMessage {
    /// Sent by a sender/scanner to request immediate responses from all receivers on LAN
    Query { sender_name: String },
    /// Sent by a receiver periodically and in response to `Query`
    ReceiverAnnounce {
        hostname: String,
        audio_port: u16,
        sample_rate: u32,
        channels: u8,
    },
}

impl DiscoveryMessage {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(96);
        buf.extend_from_slice(&DISCOVERY_MAGIC);
        match self {
            DiscoveryMessage::Query { sender_name } => {
                buf.push(1);
                let name_bytes = sender_name.as_bytes();
                let len = name_bytes.len().min(64) as u8;
                buf.push(len);
                buf.extend_from_slice(&name_bytes[..len as usize]);
            }
            DiscoveryMessage::ReceiverAnnounce {
                hostname,
                audio_port,
                sample_rate,
                channels,
            } => {
                buf.push(2);
                buf.extend_from_slice(&audio_port.to_le_bytes());
                buf.extend_from_slice(&sample_rate.to_le_bytes());
                buf.push(*channels);
                let name_bytes = hostname.as_bytes();
                let len = name_bytes.len().min(64) as u8;
                buf.push(len);
                buf.extend_from_slice(&name_bytes[..len as usize]);
            }
        }
        buf
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < 10 || buf[0..8] != DISCOVERY_MAGIC {
            return None;
        }
        match buf[8] {
            1 => {
                let len = buf[9] as usize;
                if buf.len() < 10 + len {
                    return None;
                }
                let sender_name = String::from_utf8_lossy(&buf[10..10 + len]).into_owned();
                Some(DiscoveryMessage::Query { sender_name })
            }
            2 => {
                if buf.len() < 17 {
                    return None;
                }
                let audio_port = u16::from_le_bytes([buf[9], buf[10]]);
                let sample_rate = u32::from_le_bytes([buf[11], buf[12], buf[13], buf[14]]);
                let channels = buf[15];
                let len = buf[16] as usize;
                if buf.len() < 17 + len {
                    return None;
                }
                let hostname = String::from_utf8_lossy(&buf[17..17 + len]).into_owned();
                Some(DiscoveryMessage::ReceiverAnnounce {
                    hostname,
                    audio_port,
                    sample_rate,
                    channels,
                })
            }
            _ => None,
        }
    }
}
