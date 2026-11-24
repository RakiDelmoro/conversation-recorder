//! Conversation recorder firmware entry point and status loop.

mod audio;
mod config;
mod discovery;
mod protocol;
mod queue;
mod recorder;
mod ten_vad_model;
mod uploader;
mod usb;
mod vad;
mod wifi;
mod xvf3800;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::gpio::PinDriver;
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::log::EspLogger;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use log::{debug, error, info, warn};

use crate::queue::SharedQueue;
use crate::xvf3800::VadState;

const LED_BACKLOG_BYTES: usize = 262_144;

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();

    let Peripherals {
        i2s0,
        i2c0,
        modem,
        pins,
        usb_serial,
        ..
    } = Peripherals::take()?;

    let usb_transport = config::TRANSPORT == config::Transport::Usb;

    let mut wifi = if usb_transport {
        None
    } else {
        Some(wifi::setup(
            modem,
            EspSystemEventLoop::take()?,
            EspDefaultNvsPartition::take()?,
        )?)
    };

    let vad = Arc::new(VadState::new());
    let shared = Arc::new(SharedQueue::new(config::QUEUE_BYTES)?);
    let recording = Arc::new(AtomicBool::new(false));
    let audio_ok = Arc::new(AtomicBool::new(false));
    let usb_link = Arc::new(AtomicBool::new(false));

    let mut led = PinDriver::output(pins.gpio21)?;
    led.set_high()?;

    if usb_transport {
        let shared = shared.clone();
        let link = usb_link.clone();
        std::thread::Builder::new()
            .name("usb".into())
            .stack_size(16 * 1024)
            .spawn(move || {
                if let Err(e) = usb::task(usb_serial, pins.gpio19, pins.gpio20, shared, link) {
                    error!("USB task ended: {e}");
                }
            })?;
    } else {
        let peers = Arc::new(discovery::PeerState::new());
        if config::PEER_HOST.is_empty() {
            if config::PAIRING_TOKEN.is_empty() {
                error!(
                    "PAIRING_TOKEN is empty; discovery disabled; set PEER_HOST for static mode"
                );
            } else {
                let shared = shared.clone();
                let recording = recording.clone();
                let peers = peers.clone();
                std::thread::Builder::new()
                    .name("disc".into())
                    .stack_size(8 * 1024)
                    .spawn(move || {
                        if let Err(e) = discovery::task(shared, recording, peers) {
                            error!("discovery task ended: {e}");
                        }
                    })?;
            }
        } else {
            info!(
                "discovery: static receiver {}:{}",
                config::PEER_HOST,
                config::PEER_PORT
            );
            peers.set(discovery::Peer {
                host: config::PEER_HOST.to_string(),
                port: config::PEER_PORT,
            });
        }

        let shared = shared.clone();
        std::thread::Builder::new()
            .name("upload".into())
            .stack_size(12 * 1024)
            .spawn(move || {
                if let Err(e) = uploader::task(shared, peers) {
                    error!("uploader task ended: {e}");
                }
            })?;
    }

    {
        let vad = vad.clone();
        std::thread::Builder::new()
            .name("xvf".into())
            .stack_size(6 * 1024)
            .spawn(move || {
                if let Err(e) = xvf3800::task(i2c0, pins.gpio5, pins.gpio6, vad) {
                    error!("XVF3800 task ended: {e}");
                }
            })?;
    }

    {
        let vad = vad.clone();
        let shared = shared.clone();
        let recording = recording.clone();
        let audio_ok = audio_ok.clone();
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
                ) {
                    error!("audio task ended: {e}");
                }
            })?;
    }

    let mut last_wifi_attempt: Option<Instant> = None;
    let mut wifi_was_connected = false;
    let mut last_stats = Instant::now();

    loop {
        let connected = match wifi.as_mut() {
            Some(w) => w.is_connected().unwrap_or(false),
            None => usb_link.load(Ordering::Relaxed),
        };

        if let Some(w) = wifi.as_mut() {
            if connected && !wifi_was_connected {
                info!("wifi: connected");
            } else if !connected && wifi_was_connected {
                warn!("wifi: disconnected");
            }

            if !connected
                && last_wifi_attempt.map_or(true, |t| t.elapsed() >= Duration::from_secs(5))
            {
                if let Err(e) = w.connect() {
                    debug!("wifi: reconnect attempt: {e}");
                }
                last_wifi_attempt = Some(Instant::now());
            }
        }
        wifi_was_connected = connected;

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
