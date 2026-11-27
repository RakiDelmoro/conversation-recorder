// User-tunable values. Keep hardware-variant knobs together at the bottom.

pub const I2S_RATE: u32 = 48_000;
pub const I2S_SLAVE: bool = true; // XVF3800 drives BCLK/WS
pub const I2S_BITS: u32 = 32; // v1.0.9 I2S image uses 32-bit slots
pub const I2S_CHANNEL: usize = 0; // 0 = ASR lane routed at boot, 1 = default R ASR lane

pub const SILENCE_TIMEOUT_MS: u64 = 2_000;
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
/// Minimum smoothed zero-crossing rate for VADNet speech. Low-frequency
/// rumble has a much lower ZCR than speech.
pub const SPEECH_ZCR_MIN: f32 = 0.05;
/// Smoothing window for the ZCR check.
pub const SPEECH_ZCR_WINDOW_MS: f32 = 200.0;
/// Keep the ring blue this long after the last speech block inside an open
/// segment, then turn it off.
pub const RING_SPEECH_HOLD_MS: u32 = 400;
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
