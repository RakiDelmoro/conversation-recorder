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
/// The software gate rejects VADNet speech below this absolute level.
pub const SPEECH_GATE_MIN_DBFS: f32 = -34.0;
/// The gate threshold is also raised this far above the tracked noise floor.
pub const SPEECH_GATE_MARGIN_DB: f32 = 2.0;
/// Slow noise-floor tracking, applied only while VADNet reports non-speech.
pub const SPEECH_GATE_NOISE_ALPHA: f32 = 0.01;
/// One-pole smoothing for the gate's energy estimate. At 10 ms blocks, 0.05
/// gives an approximately 200 ms window.
pub const SPEECH_GATE_SMOOTHING: f32 = 0.05;
/// Required accumulated gated speech before a block can count as speech.
/// Short gaps release this counter at twice the block rate.
pub const SPEECH_GATE_CONFIRM_MS: u32 = 300;
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
