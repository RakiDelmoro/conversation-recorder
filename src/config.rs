// User-tunable values. Keep hardware-variant knobs together at the bottom.

pub const I2S_RATE: u32 = 48_000;
pub const I2S_SLAVE: bool = true; // XVF3800 drives BCLK/WS
pub const I2S_BITS: u32 = 32; // v1.0.9 I2S image uses 32-bit slots
pub const I2S_CHANNEL: usize = 0; // 0 = ASR lane routed at boot, 1 = default R ASR lane

pub const SILENCE_TIMEOUT_MS: u64 = 60_000;
/// Speech required inside the rolling window before a segment starts.
/// Pre-roll keeps the audio before the trigger point.
pub const MIN_SPEECH_MS: u32 = 600;
/// Speech must persist this long before it resets the silence timer while a
/// segment is active. This rejects isolated non-speech blocks.
pub const SPEECH_RESUME_MS: u32 = 300;
pub const PRE_ROLL_MS: u32 = 3_000;
/// Candidate must contain at least this much active audio before it is sent.
pub const CANDIDATE_MIN_ACTIVE_MS: u32 = 500;
pub const HOLDBACK_MS: u32 = 1_500;
/// Keep the ring blue this long after the last speech block inside an open
/// segment, then switch to green for the silence wait.
pub const RING_SPEECH_HOLD_MS: u32 = 400;
/// Microphone level range mapped to ring brightness while speech is active.
pub const RING_LEVEL_MIN_DBFS: f32 = -50.0;
pub const RING_LEVEL_MAX_DBFS: f32 = -12.0;
/// Brightness floor/ceiling for the speech VU, 0-255.
pub const RING_LEVEL_FLOOR: u8 = 40;
pub const RING_LEVEL_CEIL: u8 = 255;
/// Per-10 ms-block brightness decay when the level drops, 0.0-1.0.
pub const RING_LEVEL_RELEASE: f32 = 0.80;
/// Fixed brightness for the green hold state.
pub const RING_HOLD_BRIGHTNESS: u8 = 128;
pub const SEGMENT_MS: u64 = 1_800_000;
pub const QUEUE_BYTES: usize = 4 * 1024 * 1024;

/// Test mode: record exactly once from boot, for ONE_SHOT_MS, then stop.
/// No VAD and no repeat sessions.
pub const ONE_SHOT: bool = false;
pub const ONE_SHOT_MS: u64 = 60_000;
pub const ONE_SHOT_ACK_TIMEOUT_MS: u64 = 5_000;

/// Test mode: log accepted/rejected candidates but never queue or send audio.
pub const LOG_ONLY: bool = false;

pub const XVF3800_ADDR: u8 = 0x2C;
