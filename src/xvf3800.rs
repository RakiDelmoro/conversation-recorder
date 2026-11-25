//! XVF3800 control over I2C: VAD flag, direction of arrival, boot routing.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use esp_idf_hal::delay::{FreeRtos, TickType};
use esp_idf_hal::gpio::{Gpio5, Gpio6};
use esp_idf_hal::i2c::{I2cConfig, I2cDriver, I2C0};
use esp_idf_hal::units::Hertz;
use log::{info, warn};

use crate::config;

pub struct VadState {
    speech: AtomicBool,
    doa: AtomicU32,
    polls: AtomicU32,
    errors: AtomicU32,
    ever_ok: AtomicBool,
    last_ok_ms: AtomicU32,
}

pub struct VadSnapshot {
    pub healthy: bool,
    pub speech: bool,
    pub doa: u16,
    pub polls: u32,
    pub errors: u32,
}

impl VadState {
    pub fn new() -> Self {
        Self {
            speech: AtomicBool::new(false),
            doa: AtomicU32::new(u16::MAX as u32),
            polls: AtomicU32::new(0),
            errors: AtomicU32::new(0),
            ever_ok: AtomicBool::new(false),
            last_ok_ms: AtomicU32::new(0),
        }
    }

    pub fn snapshot(&self) -> VadSnapshot {
        let ever_ok = self.ever_ok.load(Ordering::Relaxed);
        let now_ms = now_ms();
        let last = self.last_ok_ms.load(Ordering::Relaxed);
        let healthy = ever_ok && now_ms.wrapping_sub(last) < 2_000;
        VadSnapshot {
            healthy,
            speech: self.speech.load(Ordering::Relaxed),
            doa: self.doa.load(Ordering::Relaxed) as u16,
            polls: self.polls.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

fn now_ms() -> u32 {
    (unsafe { esp_idf_svc::sys::esp_timer_get_time() } / 1000) as u32
}

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

const DOA_RESID: u8 = 20;
/// I2S firmware v1.0.8 moved DOA_VALUE from command 19 to 18.
const DOA_CMD: u8 = 18;
const DOA_CMD_LEGACY: u8 = 19;

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
const LED_DOA_COLOR_CMD: u8 = 17;
const LED_EFFECT_DOA: u8 = 4;

fn set_led_doa_color(i2c: &mut I2cDriver, base: u32, doa: u32) -> Result<()> {
    let mut data = [0u8; 8];
    data[..4].copy_from_slice(&base.to_le_bytes());
    data[4..].copy_from_slice(&doa.to_le_bytes());
    xvf_write(i2c, LED_RESID, LED_DOA_COLOR_CMD, &data)
}

const AEC_RESID: u8 = 33;
const AEC_SPENERGY_CMD: u8 = 80;

fn read_speech_energy(i2c: &mut I2cDriver, out: &mut [f32; 4]) -> Result<()> {
    let mut buf = [0u8; 16];
    xvf_read(i2c, AEC_RESID, AEC_SPENERGY_CMD, &mut buf)?;
    for (i, value) in out.iter_mut().enumerate() {
        let o = i * 4;
        *value = f32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    }
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
    state: Arc<VadState>,
    recording: Arc<AtomicBool>,
) -> Result<()> {
    let cfg = I2cConfig::new().baudrate(Hertz(100_000));
    let mut i2c = I2cDriver::new(i2c0, sda, scl, &cfg)?;

    let version = match read_version(&mut i2c) {
        Ok(version) => {
            info!(
                "XVF3800 firmware v{}.{}.{}",
                version[0], version[1], version[2]
            );
            Some(version)
        }
        Err(e) => {
            warn!("XVF3800 firmware read failed: {e}; probing both DOA command layouts");
            None
        }
    };

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

    // DOA_VALUE was added in firmware v1.0.6.
    let doa_supported = version.map_or(true, |v| (v[0], v[1], v[2]) >= (1, 0, 6));
    let mut p = [0u8; 4];
    let mut doa_cmd = None;
    if doa_supported {
        let preferred_cmd = match version {
            Some(v) if (v[0], v[1], v[2]) < (1, 0, 8) => DOA_CMD_LEGACY,
            _ => DOA_CMD,
        };
        let alternate_cmd = if preferred_cmd == DOA_CMD {
            DOA_CMD_LEGACY
        } else {
            DOA_CMD
        };
        for cmd in [preferred_cmd, alternate_cmd] {
            if xvf_read(&mut i2c, DOA_RESID, cmd, &mut p).is_ok() {
                info!("XVF3800 DOA_VALUE read via cmd {}", cmd);
                doa_cmd = Some(cmd);
                break;
            }
        }
        if doa_cmd.is_none() {
            warn!(
                "XVF3800 DOA_VALUE unavailable via cmd {preferred_cmd} and {alternate_cmd}; XVF speech flag disabled"
            );
        }
    } else {
        let v = version.unwrap_or([0; 3]);
        info!(
            "XVF3800 firmware v{}.{}.{} has no DOA_VALUE; XVF speech flag disabled",
            v[0], v[1], v[2]
        );
    }
    if doa_cmd.is_some() {
        let doa = u16::from_le_bytes([p[0], p[1]]);
        let speech = u16::from_le_bytes([p[2], p[3]]) != 0;
        state.doa.store(doa as u32, Ordering::Relaxed);
        state.speech.store(speech, Ordering::Relaxed);
        state.ever_ok.store(true, Ordering::Relaxed);
        state.last_ok_ms.store(now_ms(), Ordering::Relaxed);
        state.polls.fetch_add(1, Ordering::Relaxed);
    }

    let mut led_ok = match xvf_write(
        &mut i2c,
        LED_RESID,
        LED_EFFECT_CMD,
        &[LED_EFFECT_DOA],
    )
    .and_then(|_| set_led_doa_color(&mut i2c, 0x000020, 0x00FFFF))
    {
        Ok(()) => {
            info!("XVF3800 LED ring: DoA effect (blue idle, green recording)");
            true
        }
        Err(e) => {
            warn!("XVF3800 LED ring setup failed: {e}");
            false
        }
    };
    let mut energy = [0f32; 4];
    let mut speech_via_energy = if doa_cmd.is_none() {
        match read_speech_energy(&mut i2c, &mut energy) {
            Ok(()) => {
                let speech = energy.iter().any(|v| *v > 0.0);
                state.speech.store(speech, Ordering::Relaxed);
                state.ever_ok.store(true, Ordering::Relaxed);
                state.last_ok_ms.store(now_ms(), Ordering::Relaxed);
                state.polls.fetch_add(1, Ordering::Relaxed);
                info!("XVF3800 speech energy enabled (AEC_SPENERGY_VALUES)");
                true
            }
            Err(e) => {
                warn!("XVF3800 speech energy unavailable: {e}; using ESP energy VAD");
                false
            }
        }
    } else {
        false
    };
    let mut led_recording = recording.load(Ordering::Relaxed);

    let mut last_doa_log: Option<Instant> = None;
    let mut last_poll_error_log: Option<Instant> = None;
    let mut last_routing_retry = Instant::now();

    loop {
        if let Some(cmd) = doa_cmd {
            match xvf_read(&mut i2c, DOA_RESID, cmd, &mut p) {
                Ok(()) => {
                    let doa = u16::from_le_bytes([p[0], p[1]]);
                    let speech = u16::from_le_bytes([p[2], p[3]]) != 0;
                    state.doa.store(doa as u32, Ordering::Relaxed);
                    state.speech.store(speech, Ordering::Relaxed);
                    state.ever_ok.store(true, Ordering::Relaxed);
                    state.last_ok_ms.store(now_ms(), Ordering::Relaxed);
                    state.polls.fetch_add(1, Ordering::Relaxed);

                    if speech
                        && last_doa_log
                            .map_or(true, |t| t.elapsed() >= Duration::from_secs(5))
                    {
                        if doa > 359 {
                            info!("XVF3800 speech doa=N/A");
                        } else {
                            info!("XVF3800 speech doa={} deg", doa);
                        }
                        last_doa_log = Some(Instant::now());
                    }
                }
                Err(e) => {
                    state.errors.fetch_add(1, Ordering::Relaxed);
                    state.speech.store(false, Ordering::Relaxed);
                    if last_poll_error_log
                        .map_or(true, |t| t.elapsed() >= Duration::from_secs(5))
                    {
                        warn!("XVF3800 poll failed: {e}");
                        last_poll_error_log = Some(Instant::now());
                    }
                }
            }
        } else if speech_via_energy {
            match read_speech_energy(&mut i2c, &mut energy) {
                Ok(()) => {
                    let speech = energy.iter().any(|v| *v > 0.0);
                    state.speech.store(speech, Ordering::Relaxed);
                    state.ever_ok.store(true, Ordering::Relaxed);
                    state.last_ok_ms.store(now_ms(), Ordering::Relaxed);
                    state.polls.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    state.errors.fetch_add(1, Ordering::Relaxed);
                    state.speech.store(false, Ordering::Relaxed);
                    if last_poll_error_log
                        .map_or(true, |t| t.elapsed() >= Duration::from_secs(5))
                    {
                        warn!("XVF3800 speech energy poll failed: {e}");
                        last_poll_error_log = Some(Instant::now());
                    }
                    speech_via_energy = false;
                    warn!("XVF3800 speech energy unavailable; using ESP energy VAD");
                }
            }
        }

        let rec = recording.load(Ordering::Relaxed);
        if rec != led_recording {
            if led_ok {
                let base = if rec { 0x002000 } else { 0x000020 };
                if let Err(e) = set_led_doa_color(&mut i2c, base, 0x00FFFF) {
                    warn!("XVF3800 LED update failed: {e}");
                    led_ok = false;
                }
            }
            led_recording = rec;
        }

        if !routing_ok && last_routing_retry.elapsed() >= Duration::from_secs(1) {
            if apply_routing(&mut i2c).is_ok() {
                info!("XVF3800 ASR route L=[7,3], ASROUTONOFF=1");
                routing_ok = true;
            }
            last_routing_retry = Instant::now();
        }

        FreeRtos::delay_ms(100);
    }
}
