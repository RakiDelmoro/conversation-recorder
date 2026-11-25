//! I2S capture from the XVF3800 and 48 kHz -> 16 kHz decimation.

use std::sync::atomic::{AtomicBool, Ordering};
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
use crate::ten_vad_model::TenVad;
use crate::xvf3800::VadState;

const RATE_CHECK_MS: u64 = 2_000;

pub fn task(
    i2s0: I2S0,
    bclk: Gpio8,
    ws: Gpio7,
    din: Gpio43,
    vad: Arc<VadState>,
    shared: Arc<SharedQueue>,
    recording: Arc<AtomicBool>,
    audio_ok: Arc<AtomicBool>,
) -> Result<()> {
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

    let mut model = if config::USE_TEN_VAD {
        let model = TenVad::new(config::TEN_VAD_THRESHOLD);
        if model.is_some() {
            info!(
                "TEN VAD model ready (threshold {})",
                config::TEN_VAD_THRESHOLD
            );
        } else {
            error!("TEN VAD init failed; falling back to XVF3800 VAD");
        }
        model
    } else {
        info!("TEN VAD disabled; using XVF3800 speech energy");
        None
    };
    let mut recorder = Recorder::new(recording, model.is_some())?;

    let mut raw = vec![0u8; (config::I2S_RATE as usize / 50) * frame_bytes];
    let mut mono = [0i16; 960];
    let mut phase: u8 = 0;
    let mut last_error_log: Option<Instant> = None;
    let mut rate_start: Option<(Instant, u64)> = None;
    let mut rate_checked = false;
    let mut vad_time_us: u64 = 0;
    let mut vad_blocks: u32 = 0;

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
                                "I2S rate mismatch: configured {} Hz, observed {} Hz; flash the matching XVF3800 firmware (48k master or 16k slave)",
                                config::I2S_RATE,
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
                (v >> 16) as i16
            };

            if config::I2S_RATE == 48_000 {
                // Band-limited to 16 kHz by the XVF pipeline; decimate by three.
                if phase == 0 && n_out < mono.len() {
                    mono[n_out] = s;
                    n_out += 1;
                }
                phase = (phase + 1) % 3;
            } else if n_out < mono.len() {
                mono[n_out] = s;
                n_out += 1;
            }
        }

        if n_out > 0 {
            let started = Instant::now();
            let model_probability = match model.as_mut() {
                Some(model) => model.push(&mono[..n_out]),
                None => 0.0,
            };
            if model.is_some() {
                vad_time_us += started.elapsed().as_micros() as u64;
                vad_blocks += 1;
                if vad_blocks >= 100 {
                    info!(
                        "TEN VAD: {} us per 20 ms block ({} blocks)",
                        vad_time_us / vad_blocks as u64,
                        vad_blocks
                    );
                    vad_time_us = 0;
                    vad_blocks = 0;
                }
            }
            recorder.process(&mono[..n_out], &vad, &shared, model_probability);
        }
    }
}
