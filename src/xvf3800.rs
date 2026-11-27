//! XVF3800 control over I2C: boot routing and LED ring.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use esp_idf_hal::delay::{FreeRtos, TickType};
use esp_idf_hal::gpio::{Gpio5, Gpio6};
use esp_idf_hal::i2c::{I2cConfig, I2cDriver, I2C0};
use esp_idf_hal::units::Hertz;
use log::{info, warn};

use crate::config;

fn xvf_read(i2c: &mut I2cDriver, resid: u8, cmd: u8, out: &mut [u8]) -> Result<()> {
    assert!(out.len() + 1 <= 64);
    let mut attempts = 0u32;
    loop {
        let tx = [resid, cmd | 0x80, (out.len() + 1) as u8];
        let mut rx = [0u8; 64];
        i2c.write_read(
            config::XVF3800_ADDR,
            &tx,
            &mut rx[..out.len() + 1],
            TickType::from(Duration::from_millis(50)).0,
        )?;
        let status = rx[0];
        if status == 0 {
            out.copy_from_slice(&rx[1..out.len() + 1]);
            return Ok(());
        }
        attempts += 1;
        if status == 0x40 && attempts < 8 {
            FreeRtos::delay_ms(10);
            continue;
        }
        bail!(
            "XVF3800 status {} (resid {} cmd {})",
            status,
            resid,
            cmd
        );
    }
}

fn xvf_write(i2c: &mut I2cDriver, resid: u8, cmd: u8, data: &[u8]) -> Result<()> {
    assert!(data.len() + 3 <= 16);
    let mut tx = [0u8; 16];
    tx[0] = resid;
    tx[1] = cmd;
    tx[2] = data.len() as u8;
    tx[3..3 + data.len()].copy_from_slice(data);
    i2c.write(
        config::XVF3800_ADDR,
        &tx[..3 + data.len()],
        TickType::from(Duration::from_millis(50)).0,
    )?;
    Ok(())
}

fn read_version(i2c: &mut I2cDriver) -> Result<[u8; 3]> {
    // The XVF3800 may still be booting when the firmware reaches this code.
    let mut last_error = None;
    for attempt in 0..10 {
        if attempt > 0 {
            FreeRtos::delay_ms(100);
        }
        let mut version = [0u8; 3];
        match xvf_read(i2c, 48, 0, &mut version) {
            Ok(()) => return Ok(version),
            Err(e) => last_error = Some(e),
        }
    }
    Err(last_error.expect("at least one attempt"))
}

const LED_RESID: u8 = 20;
const LED_EFFECT_CMD: u8 = 12;
const LED_BRIGHTNESS_CMD: u8 = 13;
const LED_COLOR_CMD: u8 = 16;
const LED_EFFECT_OFF: u8 = 0;
const LED_EFFECT_SOLID: u8 = 3;
const LED_SPEECH_COLOR: u32 = 0x0000FF;
const LED_HOLD_COLOR: u32 = 0x00FF00;
/// Minimum brightness change before another I2C write while speech is active.
const LED_LEVEL_STEP: u8 = 4;
const LED_LEVEL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq, Eq)]
enum RingMode {
    Off,
    Speech,
    Hold,
}

impl RingMode {
    fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Speech => "pulsing blue (speech)",
            Self::Hold => "solid green (hold)",
        }
    }
}

fn set_led_brightness(i2c: &mut I2cDriver, brightness: u8) -> Result<()> {
    xvf_write(i2c, LED_RESID, LED_BRIGHTNESS_CMD, &[brightness])
}

fn set_led_ring(i2c: &mut I2cDriver, mode: RingMode, brightness: u8) -> Result<()> {
    if mode == RingMode::Off {
        xvf_write(i2c, LED_RESID, LED_EFFECT_CMD, &[LED_EFFECT_OFF])?;
        return Ok(());
    }
    let color = match mode {
        RingMode::Speech => LED_SPEECH_COLOR,
        _ => LED_HOLD_COLOR,
    };
    // Set colour and brightness before starting the effect so the old look is
    // never visible under the new state.
    xvf_write(i2c, LED_RESID, LED_COLOR_CMD, &color.to_le_bytes())?;
    set_led_brightness(i2c, brightness)?;
    xvf_write(i2c, LED_RESID, LED_EFFECT_CMD, &[LED_EFFECT_SOLID])?;
    Ok(())
}

fn apply_routing(i2c: &mut I2cDriver) -> Result<()> {
    xvf_write(i2c, 33, 35, &1i32.to_le_bytes())?;
    xvf_write(i2c, 35, 15, &[7, 3])?;
    let mut rb = [0u8; 2];
    xvf_read(i2c, 35, 15, &mut rb)?;
    if rb != [7, 3] {
        bail!("AUDIO_MGR_OP_L read back as {:?}", rb);
    }
    Ok(())
}

pub fn task(
    i2c0: I2C0,
    sda: Gpio5,
    scl: Gpio6,
    recording: Arc<AtomicBool>,
    ring_speech: Arc<AtomicBool>,
    ring_level: Arc<AtomicU8>,
) -> Result<()> {
    let cfg = I2cConfig::new().baudrate(Hertz(100_000));
    let mut i2c = I2cDriver::new(i2c0, sda, scl, &cfg)?;

    let mut led_ok = match set_led_ring(&mut i2c, RingMode::Off, 0) {
        Ok(()) => {
            info!("XVF3800 LED ring: off until recording");
            true
        }
        Err(e) => {
            warn!("XVF3800 LED ring setup failed: {e}");
            false
        }
    };

    match read_version(&mut i2c) {
        Ok(version) => info!(
            "XVF3800 firmware v{}.{}.{}",
            version[0], version[1], version[2]
        ),
        Err(e) => warn!("XVF3800 firmware read failed: {e}"),
    }

    let mut routing_ok = match apply_routing(&mut i2c) {
        Ok(()) => {
            info!("XVF3800 ASR route L=[7,3], ASROUTONOFF=1");
            true
        }
        Err(e) => {
            warn!("XVF3800 ASR routing failed: {e}; using firmware default outputs");
            false
        }
    };

    let mut led_mode = RingMode::Off;
    let mut led_level = 0u8;
    let mut last_level_write = Instant::now();
    let mut last_led_retry = Instant::now();
    let mut last_routing_retry = Instant::now();

    loop {
        let mode = if recording.load(Ordering::Relaxed) {
            if ring_speech.load(Ordering::Relaxed) {
                RingMode::Speech
            } else {
                RingMode::Hold
            }
        } else {
            RingMode::Off
        };
        let brightness = match mode {
            RingMode::Off => 0,
            RingMode::Speech => ring_level.load(Ordering::Relaxed).max(config::RING_LEVEL_FLOOR),
            RingMode::Hold => config::RING_HOLD_BRIGHTNESS,
        };

        if mode != led_mode {
            if led_ok {
                match set_led_ring(&mut i2c, mode, brightness) {
                    Ok(()) => {
                        led_mode = mode;
                        led_level = brightness;
                        last_level_write = Instant::now();
                        info!("XVF3800 LED ring: {}", mode.label());
                    }
                    Err(e) => {
                        warn!("XVF3800 LED update failed: {e}");
                        led_ok = false;
                        led_mode = mode;
                        last_led_retry = Instant::now();
                    }
                }
            } else {
                led_mode = mode;
            }
        } else if !led_ok && last_led_retry.elapsed() >= Duration::from_secs(1) {
            if set_led_ring(&mut i2c, led_mode, brightness).is_ok() {
                info!("XVF3800 LED ring control recovered");
                led_ok = true;
                led_level = brightness;
                last_level_write = Instant::now();
            }
            last_led_retry = Instant::now();
        } else if mode == RingMode::Speech
            && led_ok
            && brightness != led_level
            && (brightness.abs_diff(led_level) >= LED_LEVEL_STEP
                || last_level_write.elapsed() >= LED_LEVEL_INTERVAL)
        {
            match set_led_brightness(&mut i2c, brightness) {
                Ok(()) => {
                    led_level = brightness;
                    last_level_write = Instant::now();
                }
                Err(e) => {
                    warn!("XVF3800 LED brightness failed: {e}");
                    led_ok = false;
                    last_led_retry = Instant::now();
                }
            }
        }

        if !routing_ok && last_routing_retry.elapsed() >= Duration::from_secs(1) {
            if apply_routing(&mut i2c).is_ok() {
                info!("XVF3800 ASR route L=[7,3], ASROUTONOFF=1");
                routing_ok = true;
            }
            last_routing_retry = Instant::now();
        }

        FreeRtos::delay_ms(50);
    }
}
