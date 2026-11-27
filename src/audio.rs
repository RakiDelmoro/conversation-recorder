//! I2S capture from the XVF3800 and 48 kHz -> 16 kHz decimation.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use esp_idf_hal::delay::{FreeRtos, TickType};
use esp_idf_hal::gpio::{AnyIOPin, Gpio43, Gpio7, Gpio8};
use esp_idf_hal::i2s::config::{
    Config as I2sConfig, DataBitWidth, Role, SlotMode, StdClkConfig, StdConfig, StdGpioConfig,
    StdSlotConfig,
};
use esp_idf_hal::i2s::{I2sDriver, I2S0};
use log::{error, info, warn};

use crate::config;
use crate::queue::SharedQueue;
use crate::recorder::Recorder;
use crate::vadnet::VadNet;

const RATE_CHECK_MS: u64 = 2_000;

struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    fn lowpass(fs: f64, fc: f64, q: f64) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * fc / fs;
        let cos_w0 = w0.cos();
        let alpha = w0.sin() / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: ((1.0 - cos_w0) / 2.0) / a0,
            b1: (1.0 - cos_w0) / a0,
            b2: ((1.0 - cos_w0) / 2.0) / a0,
            a1: (-2.0 * cos_w0) / a0,
            a2: (1.0 - alpha) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Sixth-order Butterworth low-pass. Applied before decimation so energy
/// above 8 kHz cannot fold back into the 16 kHz output, while preserving
/// more of the 6-7 kHz speech band than the previous 6.5 kHz filter.
struct LowPass {
    s1: Biquad,
    s2: Biquad,
    s3: Biquad,
    fs: f64,
    fc: f64,
}

/// Maps the speech envelope to an LED brightness with fast attack and a slow
/// release, so the ring pulses with speech instead of flickering per block.
struct LevelMeter {
    env: f32,
}

impl LevelMeter {
    fn new() -> Self {
        Self { env: 0.0 }
    }

    fn process(&mut self, pcm: &[i16]) -> u8 {
        if pcm.is_empty() {
            return config::RING_LEVEL_FLOOR;
        }
        let mut sum = 0.0f64;
        for &s in pcm {
            let v = s as f64;
            sum += v * v;
        }
        let rms = (sum / pcm.len() as f64).sqrt();
        let db = 20.0 * (rms / 32768.0).max(1e-9).log10() as f32;
        let span = config::RING_LEVEL_MAX_DBFS - config::RING_LEVEL_MIN_DBFS;
        let target =
            ((db - config::RING_LEVEL_MIN_DBFS) / span).clamp(0.0, 1.0);
        self.env = if target > self.env {
            target
        } else {
            self.env * config::RING_LEVEL_RELEASE
        };
        let floor = config::RING_LEVEL_FLOOR as f32;
        let ceil = config::RING_LEVEL_CEIL as f32;
        (floor + self.env * (ceil - floor))
            .round()
            .clamp(0.0, 255.0) as u8
    }
}

impl LowPass {
    fn new(fs: f64, fc: f64) -> Self {
        Self {
            s1: Biquad::lowpass(fs, fc, 0.517_638_09),
            s2: Biquad::lowpass(fs, fc, 0.707_106_78),
            s3: Biquad::lowpass(fs, fc, 1.931_851_65),
            fs,
            fc,
        }
    }

    fn set_rate(&mut self, fs: f64) {
        if (fs - self.fs).abs() > 1.0 {
            let fc = self.fc;
            *self = Self::new(fs, fc);
        }
    }

    fn process(&mut self, x: f64) -> f64 {
        self.s1.process(self.s2.process(self.s3.process(x)))
    }
}

pub fn task(
    i2s0: I2S0,
    bclk: Gpio8,
    ws: Gpio7,
    din: Gpio43,
    shared: Arc<SharedQueue>,
    recording: Arc<AtomicBool>,
    audio_ok: Arc<AtomicBool>,
    acked: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    ring_speech: Arc<AtomicBool>,
    ring_level: Arc<AtomicU8>,
) -> Result<()> {
    let mut vadnet = match VadNet::new() {
        Some(vad) => vad,
        None => anyhow::bail!(
            "VADNet init failed; check the model partition and srmodels.bin"
        ),
    };

    let bits = match config::I2S_BITS {
        16 => DataBitWidth::Bits16,
        32 => DataBitWidth::Bits32,
        other => anyhow::bail!("unsupported I2S_BITS: {other}"),
    };
    let sample_bytes = (config::I2S_BITS / 8) as usize;
    let frame_bytes = sample_bytes * 2;
    let chan = I2sConfig::new()
        .role(if config::I2S_SLAVE {
            Role::Target
        } else {
            Role::Controller
        })
        .dma_buffer_count(8)
        // One DMA descriptor is limited to 4095 bytes. Keep each buffer at
        // 10 ms so the request is not silently clamped by the IDF driver.
        .frames_per_buffer((config::I2S_RATE / 100).min(480));
    let std_cfg = StdConfig::new(
        chan,
        StdClkConfig::from_sample_rate_hz(config::I2S_RATE),
        StdSlotConfig::philips_slot_default(bits, SlotMode::Stereo),
        StdGpioConfig::default(),
    );
    let mut i2s = I2sDriver::new_std_rx(i2s0, &std_cfg, bclk, din, None::<AnyIOPin>, ws)?;
    // esp-idf-hal only initializes the channel; reads fail with
    // ESP_ERR_INVALID_STATE until the RX channel is explicitly enabled.
    i2s.rx_enable()?;
    audio_ok.store(true, Ordering::Relaxed);

    let mut recorder = Recorder::new(recording, acked, restart, ring_speech)?;

    let mut raw = vec![0u8; (config::I2S_RATE as usize / 50) * frame_bytes];
    let mut mono = [0i16; 960];
    let mut input_rate = config::I2S_RATE as f64;
    let mut lowpass = LowPass::new(input_rate, 7_200.0);
    let mut meter = LevelMeter::new();
    let mut resample_t = 0.0f64;
    let mut last_error_log: Option<Instant> = None;
    let mut rate_start: Option<(Instant, u64)> = None;
    let mut rate_checked = false;

    loop {
        let n = match i2s.read(
            &mut raw,
            TickType::from(Duration::from_millis(1000)).0,
        ) {
            Ok(n) => n,
            Err(e) => {
                if e.code() as u32 == esp_idf_sys::ESP_ERR_TIMEOUT as u32 {
                    if last_error_log.map_or(true, |t| t.elapsed() >= Duration::from_secs(5)) {
                        warn!(
                            "I2S read timed out: no BCLK/WS. Use XVF master 48k firmware with I2S_SLAVE=true, or XVF slave 16k with I2S_SLAVE=false and I2S_RATE=16000"
                        );
                        last_error_log = Some(Instant::now());
                    }
                    FreeRtos::delay_ms(100);
                    continue;
                }
                return Err(e.into());
            }
        };

        if n == 0 {
            FreeRtos::delay_ms(10);
            continue;
        }
        if n % frame_bytes != 0 {
            warn!(
                "I2S read returned {} bytes, not a whole {}-bit stereo frame",
                n, config::I2S_BITS
            );
        }
        let frames = n / frame_bytes;

        // Rate self-check over the first two seconds of successful reads.
        if !rate_checked {
            match rate_start {
                None => rate_start = Some((Instant::now(), frames as u64)),
                Some((start, mut total)) => {
                    total += frames as u64;
                    let elapsed = start.elapsed();
                    if elapsed >= Duration::from_millis(RATE_CHECK_MS) {
                        let observed = total * 1000 / elapsed.as_millis().max(1) as u64;
                        info!(
                            "I2S configured {} Hz, observed {} Hz",
                            config::I2S_RATE,
                            observed
                        );
                        let delta = observed.abs_diff(config::I2S_RATE as u64);
                        if delta * 10 > config::I2S_RATE as u64 {
                            error!(
                                "I2S rate mismatch: configured {} Hz, observed {} Hz; resampling to 16000 Hz",
                                config::I2S_RATE,
                                observed
                            );
                        }
                        if delta * 50 > config::I2S_RATE as u64 {
                            input_rate = observed as f64;
                            lowpass.set_rate(input_rate);
                            info!(
                                "I2S input measured at {} Hz; resampling to 16000 Hz",
                                observed
                            );
                        }
                        rate_checked = true;
                    } else {
                        rate_start = Some((start, total));
                    }
                }
            }
        }

        let mut n_out = 0usize;
        for i in 0..frames {
            let o = i * frame_bytes + config::I2S_CHANNEL * sample_bytes;
            let s = if sample_bytes == 2 {
                i16::from_le_bytes([raw[o], raw[o + 1]])
            } else {
                let v = i32::from_le_bytes([raw[o], raw[o + 1], raw[o + 2], raw[o + 3]]);
                (v >> 17) as i16
            };

            let filtered = lowpass.process(s as f64);
            let s = filtered
                .round()
                .clamp(i16::MIN as f64, i16::MAX as f64) as i16;

            resample_t += 16_000.0 / input_rate;
            if resample_t >= 1.0 {
                resample_t -= 1.0;
                if n_out < mono.len() {
                    mono[n_out] = s;
                    n_out += 1;
                }
            }
        }

        if n_out > 0 {
            ring_level.store(meter.process(&mono[..n_out]), Ordering::Relaxed);
            let vadnet_speech = vadnet.push(&mono[..n_out]);
            recorder.process(&mono[..n_out], &shared, vadnet_speech);
        }
    }
}
