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
/// segment is active. This rejects isolated noise blocks that flicker above
/// the energy floor.
pub const SPEECH_RESUME_MS: u32 = 300;
pub const PRE_ROLL_MS: u32 = 3_000;
/// Candidate must contain at least this much active audio before it is sent.
pub const CANDIDATE_MIN_ACTIVE_MS: u32 = 500;
pub const HOLDBACK_MS: u32 = 1_500;
pub const SEGMENT_MS: u64 = 1_800_000;
pub const QUEUE_BYTES: usize = 4 * 1024 * 1024;

/// Test mode: record exactly once from boot, for ONE_SHOT_MS, then stop.
/// No VAD and no repeat sessions.
pub const ONE_SHOT: bool = false;
pub const ONE_SHOT_MS: u64 = 60_000;
pub const ONE_SHOT_ACK_TIMEOUT_MS: u64 = 5_000;

/// Start threshold. Higher values reject more non-speech but can miss quiet
/// speech. The same value is passed to the TEN VAD model.
pub const TEN_VAD_THRESHOLD: f32 = 0.35;
/// Once a segment is active, keep it alive at a lower threshold so natural
/// pauses in speech do not chop the recording.
pub const TEN_VAD_CONTINUE_THRESHOLD: f32 = 0.20;
/// When the XVF3800 is healthy, require its speech flag to agree before
/// starting a segment. Firmware v1.0.5 has no DOA_VALUE, so its fallback is
/// only raw speech energy and is not a reliable VAD; leave this off there.
pub const REQUIRE_XVF_SPEECH: bool = false;
/// TEN VAD weights are moved to PSRAM (CONFIG_SPIRAM_RODATA) and the CPU runs
/// at 240 MHz. If the measured cost is still above 20 ms per block, set this
/// to false to use the XVF3800 speech energy instead.
pub const USE_TEN_VAD: bool = false;
/// Use ESP-SR VADNet as an independent speech veto.
pub const USE_VADNET: bool = true;
/// Test mode: log accepted/rejected candidates but never queue or send audio.
pub const LOG_ONLY: bool = false;

pub const XVF3800_ADDR: u8 = 0x2C;
