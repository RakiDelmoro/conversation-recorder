//! Conversation recorder firmware entry point and status loop.

mod audio;
mod config;
mod protocol;
mod queue;
mod recorder;
mod ten_vad_model;
mod usb;
mod vad;
mod vadnet;
mod xvf3800;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::gpio::PinDriver;
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_svc::log::EspLogger;
use log::{error, info};

use crate::queue::SharedQueue;
use crate::xvf3800::VadState;

const LED_BACKLOG_BYTES: usize = 262_144;

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();

    let Peripherals {
        i2s0,
        i2c0,
        pins,
        usb_serial,
        ..
    } = Peripherals::take()?;

    let vad = Arc::new(VadState::new());
    let shared = Arc::new(SharedQueue::new(config::QUEUE_BYTES)?);
    let recording = Arc::new(AtomicBool::new(false));
    let audio_ok = Arc::new(AtomicBool::new(false));
    let usb_link = Arc::new(AtomicBool::new(false));
    let acked = Arc::new(AtomicBool::new(false));
    let restart = Arc::new(AtomicBool::new(false));
    let speech_active = Arc::new(AtomicBool::new(false));

    let mut led = PinDriver::output(pins.gpio21)?;
    led.set_high()?;

    {
        let shared = shared.clone();
        let link = usb_link.clone();
        let acked = acked.clone();
        let restart = restart.clone();
        std::thread::Builder::new()
            .name("usb".into())
            .stack_size(16 * 1024)
            .spawn(move || {
                if let Err(e) = usb::task(
                    usb_serial,
                    pins.gpio19,
                    pins.gpio20,
                    shared,
                    link,
                    acked,
                    restart,
                ) {
                    error!("USB task ended: {e}");
                }
            })?;
    }

    {
        let vad = vad.clone();
        let speech_active = speech_active.clone();
        std::thread::Builder::new()
            .name("xvf".into())
            .stack_size(6 * 1024)
            .spawn(move || {
                if let Err(e) =
                    xvf3800::task(i2c0, pins.gpio5, pins.gpio6, vad, speech_active)
                {
                    error!("XVF3800 task ended: {e}");
                }
            })?;
    }

    {
        let vad = vad.clone();
        let shared = shared.clone();
        let recording = recording.clone();
        let audio_ok = audio_ok.clone();
        let acked = acked.clone();
        let restart = restart.clone();
        let speech_active = speech_active.clone();
        std::thread::Builder::new()
            .name("audio".into())
            .stack_size(64 * 1024)
            .spawn(move || {
                if let Err(e) = audio::task(
                    i2s0,
                    pins.gpio8,
                    pins.gpio7,
                    pins.gpio43,
                    vad,
                    shared,
                    recording,
                    audio_ok,
                    acked,
                    restart,
                    speech_active,
                ) {
                    error!("audio task ended: {e}");
                }
            })?;
    }

    let mut last_stats = Instant::now();

    loop {
        let connected = usb_link.load(Ordering::Relaxed);
        let backlog = shared.bytes();
        let is_recording = recording.load(Ordering::Relaxed);
        let (on_ms, period_ms) = if !audio_ok.load(Ordering::Relaxed) || !connected {
            (100u64, 200u64)
        } else if backlog > LED_BACKLOG_BYTES {
            (500, 1_000)
        } else if is_recording {
            (1, 1)
        } else {
            (20, 2_000)
        };
        let t_ms = (unsafe { esp_idf_svc::sys::esp_timer_get_time() } / 1000) as u64;
        if t_ms % period_ms < on_ms {
            led.set_low()?;
        } else {
            led.set_high()?;
        }

        if last_stats.elapsed() >= Duration::from_secs(30) {
            let psram_free = unsafe {
                esp_idf_svc::sys::heap_caps_get_free_size(esp_idf_svc::sys::MALLOC_CAP_SPIRAM)
            };
            let vad_stats = vad.snapshot();
            info!(
                "app: queue={} B, psram_free={} B, recording={}, vad_polls={}, vad_errors={}",
                backlog, psram_free, is_recording, vad_stats.polls, vad_stats.errors
            );
            last_stats = Instant::now();
        }

        FreeRtos::delay_ms(200);
    }
}
