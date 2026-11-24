// User-tunable values. Keep hardware-variant knobs together at the bottom.

/// Link that carries records to the host.
#[allow(dead_code)] // one variant is selected by `TRANSPORT` at compile time
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Native USB port (USB-CDC). No Wi-Fi, no discovery.
    Usb,
    /// Wi-Fi station plus UDP discovery and the TCP uploader.
    Tcp,
}

/// Active transport. Switch to `Tcp` for the wireless path.
pub const TRANSPORT: Transport = Transport::Usb;

pub const WIFI_SSID: &str = "";
pub const WIFI_PASSWORD: &str = "";
/// Optional static receiver. Empty means discover a receiver over UDP.
pub const PEER_HOST: &str = "";
pub const PEER_PORT: u16 = 8000;
/// Shared secret required in discovery offers. Empty disables discovery.
pub const PAIRING_TOKEN: &str = "";
pub const DISCOVERY_PORT: u16 = 8001;

pub const I2S_RATE: u32 = 48_000;
pub const I2S_SLAVE: bool = true; // XVF3800 drives BCLK/WS
pub const I2S_CHANNEL: usize = 0; // 0 = ASR lane routed at boot, 1 = default R ASR lane

pub const SILENCE_TIMEOUT_MS: u64 = 3_000;
pub const MIN_SPEECH_MS: u32 = 250;
pub const PRE_ROLL_MS: u32 = 1_500;
pub const HOLDBACK_MS: u32 = 1_500;
pub const SEGMENT_MS: u64 = 60_000;
pub const QUEUE_BYTES: usize = 4 * 1024 * 1024;

pub const TEN_VAD_THRESHOLD: f32 = 0.65;

pub const XVF3800_ADDR: u8 = 0x2C;
