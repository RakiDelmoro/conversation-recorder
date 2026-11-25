//! USB CDC transport: streams START/AUDIO/END records over the native USB
//! port instead of Wi-Fi/TCP. The host acknowledges samples the same way, so
//! the PSRAM queue keeps identical backpressure semantics.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use esp_idf_hal::delay::{FreeRtos, TickType, TickType_t};
use esp_idf_hal::gpio::{Gpio19, Gpio20};
use esp_idf_hal::usb_serial::{UsbSerialConfig, UsbSerialDriver, USB_SERIAL};
use log::{info, warn};

use crate::protocol;
use crate::queue::SharedQueue;

/// TX ring must hold a whole record (12-byte header + 3200-byte AUDIO) so a
/// record is queued in one short write instead of interleaving with log lines.
const TX_BYTES: usize = 8 * 1024;
const RX_BYTES: usize = 512;
const WRITE_TIMEOUT_MS: u64 = 200;
const POLL_MS: u64 = 200;
const ACK_BUF: usize = 64;

extern "C" {
    /// Routes the VFS console through the driver's TX ring. esp-idf-sys does
    /// not bind it for IDF 5.x (it only includes the new header on IDF 6+).
    fn usb_serial_jtag_vfs_use_driver();
}

struct Ctx {
    id: u32,
    start_uptime_ms: u32,
    acked: u32,
    sent: u32,
    started: bool,
    end_sent: bool,
    end_ack: bool,
    end_attempts: u8,
}

pub fn task(
    usb: USB_SERIAL<'static>,
    d_min: Gpio19<'static>,
    d_plus: Gpio20<'static>,
    shared: Arc<SharedQueue>,
    link: Arc<AtomicBool>,
) -> Result<()> {
    let mut dev = UsbSerialDriver::new(
        usb,
        d_min,
        d_plus,
        &UsbSerialConfig::new()
            .tx_buffer_size(TX_BYTES)
            .rx_buffer_size(RX_BYTES),
    )?;
    // Without this the console writes straight to the TX FIFO, bypassing the
    // driver's mutex, and a log line can split a record mid-payload.
    unsafe { usb_serial_jtag_vfs_use_driver() };
    let write_timeout = TickType::new_millis(WRITE_TIMEOUT_MS).ticks();
    let device_id = device_id();
    let mut frame = [0u8; protocol::HDR_LEN + 3200];
    let mut rx = [0u8; ACK_BUF];
    let mut rx_len = 0usize;
    let mut ctx: Option<Ctx> = None;
    let mut retry_at = Instant::now();
    let mut linked = false;

    loop {
        shared.wait(Duration::from_millis(POLL_MS));

        let connected = dev.is_connected();
        link.store(connected, Ordering::Relaxed);
        if connected != linked {
            linked = connected;
            if connected {
                info!("usb: host attached");
            } else {
                warn!("usb: host detached");
            }
        }

        if !connected {
            // The receiver loses its stream when the cable drops; rewind so
            // START(last ACK) makes it truncate the partial WAV and resume.
            if let Some(c) = ctx.as_mut() {
                if c.sent != c.acked {
                    info!("usb: resuming at sample {} after detach", c.acked);
                }
                c.sent = c.acked;
                c.started = false;
                c.end_sent = false;
                c.end_ack = false;
            }
            rx_len = 0;
            continue;
        }

        if Instant::now() < retry_at {
            continue;
        }

        if let Some(c) = ctx.as_mut() {
            if let Err(e) = drain_acks(&mut dev, &mut rx, &mut rx_len, c, &shared) {
                warn!("usb: {e}");
            }
        }

        if ctx.is_none() {
            if let Some(s) = shared.take_start() {
                ctx = Some(Ctx {
                    id: s.session_id,
                    start_uptime_ms: s.start_uptime_ms,
                    acked: 0,
                    sent: 0,
                    started: false,
                    end_sent: false,
                    end_ack: false,
                    end_attempts: 0,
                });
            }
        }

        let Some(c) = ctx.as_mut() else {
            continue;
        };

        if !c.started {
            let start = protocol::encode_start(&protocol::Start {
                session_id: c.id,
                first_sample: c.acked,
                sample_rate: 16_000,
                channels: 1,
                bits: 16,
                start_unix_ms: 0,
                device_uptime_ms: c.start_uptime_ms as u64,
                device_id,
            });
            if let Err(e) = write_all(&mut dev, &start, write_timeout) {
                warn!("usb: START failed: {e}");
                retry_at = Instant::now() + Duration::from_millis(WRITE_TIMEOUT_MS);
                continue;
            }
            info!(
                "usb: session 0x{:08x} started at sample {}",
                c.id, c.acked
            );
            c.started = true;
        }

        loop {
            let (n, next) = {
                let (hdr, body) = frame.split_at_mut(protocol::HDR_LEN);
                let pcm = unsafe {
                    std::slice::from_raw_parts_mut(body.as_mut_ptr() as *mut i16, body.len() / 2)
                };
                let Some(a) = shared.find_audio(c.id, c.sent, pcm) else {
                    break;
                };
                hdr.copy_from_slice(&protocol::header(
                    protocol::AUDIO,
                    (a.copy_len * 2) as u32,
                ));
                (
                    protocol::HDR_LEN + a.copy_len * 2,
                    a.first_sample + a.slot_len as u32,
                )
            };
            if let Err(e) = write_all(&mut dev, &frame[..n], write_timeout) {
                // A failed write queues nothing, so `sent` still names the
                // first record the host has not received. Do not rewind: the
                // ring still holds records the host will read.
                warn!("usb: send stalled: {e}");
                retry_at = Instant::now() + Duration::from_millis(WRITE_TIMEOUT_MS);
                break;
            }
            c.sent = next;
        }

        if !c.started {
            continue;
        }

        let Some(e) = shared.find_end(c.id) else {
            continue;
        };
        if c.sent < e.total_samples || c.acked < e.total_samples {
            continue;
        }

        if !c.end_sent {
            let end = protocol::encode_end(&protocol::End {
                session_id: c.id,
                total_samples: e.total_samples,
                reason: e.reason,
            });
            if let Err(err) = write_all(&mut dev, &end, write_timeout) {
                warn!("usb: END stalled: {err}");
                retry_at = Instant::now() + Duration::from_millis(WRITE_TIMEOUT_MS);
                continue;
            }
            c.end_sent = true;
            c.end_ack = false;
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while !c.end_ack && Instant::now() < deadline {
            if let Err(err) = drain_acks(&mut dev, &mut rx, &mut rx_len, c, &shared) {
                warn!("usb: {err}");
                break;
            }
            if !c.end_ack {
                FreeRtos::delay_ms(20);
            }
        }

        if c.end_ack {
            info!(
                "usb: session 0x{:08x} ended at {} samples",
                c.id, e.total_samples
            );
            shared.pop_end(c.id, e.total_samples);
            ctx = None;
            rx_len = 0;
        } else {
            c.end_attempts += 1;
            c.end_sent = false;
            c.end_ack = false;
            if c.end_attempts >= 3 {
                warn!(
                    "usb: END not acknowledged for session 0x{:08x}; dropping",
                    c.id
                );
                shared.pop_end(c.id, e.total_samples);
                ctx = None;
            } else {
                retry_at = Instant::now() + Duration::from_millis(500);
            }
        }
    }
}

fn write_all(dev: &mut UsbSerialDriver<'_>, mut buf: &[u8], timeout: TickType_t) -> Result<()> {
    while !buf.is_empty() {
        let n = dev.write(buf, timeout)?;
        if n == 0 {
            bail!("write timed out");
        }
        buf = &buf[n..];
    }
    Ok(())
}

fn drain_acks(
    dev: &mut UsbSerialDriver<'_>,
    rx: &mut [u8],
    rx_len: &mut usize,
    c: &mut Ctx,
    shared: &SharedQueue,
) -> Result<()> {
    loop {
        let mut read_any = false;
        while *rx_len < rx.len() {
            let n = dev.read(&mut rx[*rx_len..], 0)?;
            if n == 0 {
                break;
            }
            *rx_len += n;
            read_any = true;
        }

        while *rx_len >= protocol::HDR_LEN {
            match protocol::take_ack(rx, rx_len) {
                Ok(Some(ack)) => {
                    if ack.session_id == c.id {
                        c.acked = c.acked.max(ack.acked_samples);
                        shared.ack(c.id, c.acked);
                        if ack.final_ack {
                            c.end_ack = true;
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    warn!("usb: discarding {} ACK bytes: {e}", *rx_len);
                    *rx_len = 0;
                    break;
                }
            }
        }

        if !read_any {
            return Ok(());
        }
    }
}

fn device_id() -> [u8; 16] {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut mac = [0u8; 6];
    unsafe {
        esp_idf_svc::sys::esp_read_mac(
            mac.as_mut_ptr(),
            esp_idf_svc::sys::esp_mac_type_t_ESP_MAC_WIFI_STA,
        );
    }
    let mut id = [0u8; 16];
    for (i, b) in mac.iter().enumerate() {
        id[i * 2] = HEX[(b >> 4) as usize];
        id[i * 2 + 1] = HEX[(b & 0x0f) as usize];
    }
    id
}
