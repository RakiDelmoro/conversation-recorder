// User-tunable values. Keep hardware-variant knobs together at the bottom.

pub const I2S_RATE: u32 = 48_000;
pub const I2S_SLAVE: bool = true; // XVF3800 drives BCLK/WS
pub const I2S_BITS: u32 = 16; // v1.0.5 master image uses 16-bit slots; v1.0.9 uses 32
pub const I2S_CHANNEL: usize = 0; // 0 = ASR lane routed at boot, 1 = default R ASR lane

pub const SILENCE_TIMEOUT_MS: u64 = 3_000;
pub const MIN_SPEECH_MS: u32 = 250;
pub const PRE_ROLL_MS: u32 = 1_500;
pub const HOLDBACK_MS: u32 = 1_500;
pub const SEGMENT_MS: u64 = 60_000;
pub const QUEUE_BYTES: usize = 4 * 1024 * 1024;

pub const TEN_VAD_THRESHOLD: f32 = 0.65;
/// TEN VAD weights are moved to PSRAM (CONFIG_SPIRAM_RODATA) and the CPU runs
/// at 240 MHz. If the measured cost is still above 20 ms per block, set this
/// to false to use the XVF3800 speech energy instead.
pub const USE_TEN_VAD: bool = true;

pub const XVF3800_ADDR: u8 = 0x2C;
