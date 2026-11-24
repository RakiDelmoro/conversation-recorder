// Wire protocol. All fields little-endian.

pub const HDR_LEN: usize = 12;
pub const START_LEN: usize = 48;
pub const END_LEN: usize = 16;
pub const ACK_LEN: usize = 8;
pub const MAGIC: u32 = 0x4345_5243;
pub const VERSION: u8 = 1;
pub const START: u8 = 1;
pub const AUDIO: u8 = 2;
pub const END: u8 = 3;
pub const ACK: u8 = 0x81;

pub fn header(typ: u8, payload_len: u32) -> [u8; HDR_LEN] {
    let mut h = [0u8; HDR_LEN];
    h[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    h[4] = VERSION;
    h[5] = typ;
    h[8..12].copy_from_slice(&payload_len.to_le_bytes());
    h
}

pub struct Start {
    pub session_id: u32,
    pub first_sample: u32,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    pub start_unix_ms: u64,
    pub device_uptime_ms: u64,
    pub device_id: [u8; 16],
}

pub fn encode_start(s: &Start) -> [u8; HDR_LEN + START_LEN] {
    let mut out = [0u8; HDR_LEN + START_LEN];
    out[..HDR_LEN].copy_from_slice(&header(START, START_LEN as u32));
    let p = &mut out[HDR_LEN..];
    p[0..4].copy_from_slice(&s.session_id.to_le_bytes());
    p[4..8].copy_from_slice(&s.first_sample.to_le_bytes());
    p[8..12].copy_from_slice(&s.sample_rate.to_le_bytes());
    p[12..14].copy_from_slice(&s.channels.to_le_bytes());
    p[14..16].copy_from_slice(&s.bits.to_le_bytes());
    p[16..24].copy_from_slice(&s.start_unix_ms.to_le_bytes());
    p[24..32].copy_from_slice(&s.device_uptime_ms.to_le_bytes());
    p[32..48].copy_from_slice(&s.device_id);
    out
}

pub struct End {
    pub session_id: u32,
    pub total_samples: u32,
    pub reason: u8,
}

pub fn encode_end(e: &End) -> [u8; HDR_LEN + END_LEN] {
    let mut out = [0u8; HDR_LEN + END_LEN];
    out[..HDR_LEN].copy_from_slice(&header(END, END_LEN as u32));
    let p = &mut out[HDR_LEN..];
    p[0..4].copy_from_slice(&e.session_id.to_le_bytes());
    p[4..8].copy_from_slice(&e.total_samples.to_le_bytes());
    p[8] = e.reason;
    out
}

pub struct Ack {
    pub session_id: u32,
    pub acked_samples: u32,
}

pub fn decode_ack(payload: &[u8]) -> Option<Ack> {
    if payload.len() != ACK_LEN {
        return None;
    }
    Some(Ack {
        session_id: u32::from_le_bytes(payload[0..4].try_into().ok()?),
        acked_samples: u32::from_le_bytes(payload[4..8].try_into().ok()?),
    })
}
