//! TCP uploader: START/AUDIO/END records, ACK-driven resumption.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use esp_idf_hal::delay::FreeRtos;
use log::{error, info, warn};

use crate::discovery::{device_id_bytes, Peer, PeerState};
use crate::protocol;
use crate::queue::SharedQueue;

struct Ctx {
    id: u32,
    start_uptime_ms: u32,
    acked: u32,
    sent: u32,
    end_attempts: u8,
    end_sent: bool,
    end_ack: bool,
}

pub fn task(shared: Arc<SharedQueue>, peers: Arc<PeerState>) -> Result<()> {
    let device_id = device_id_bytes();
    let mut ctx: Option<Ctx> = None;
    let mut sock: Option<TcpStream> = None;
    let mut rx = [0u8; 64];
    let mut rx_len = 0usize;
    let mut backoff_ms = 1_000u64;
    let mut retry_at = Instant::now();
    let mut last_peer = peers.get();

    loop {
        shared.wait(Duration::from_millis(500));

        let current_peer = peers.get();
        if current_peer != last_peer {
            last_peer = current_peer;
            retry_at = Instant::now();
            backoff_ms = 1_000;
        }

        if let (Some(s), Some(c)) = (sock.as_mut(), ctx.as_mut()) {
            if let Err(e) = drain_acks(s, &mut rx, &mut rx_len, c, &shared) {
                warn!("uploader: connection lost: {e}");
                drop(sock.take());
                c.sent = c.acked;
                c.end_sent = false;
                c.end_ack = false;
            }
        }

        if ctx.is_none() {
            if let Some(s) = shared.take_start() {
                ctx = Some(Ctx {
                    id: s.session_id,
                    start_uptime_ms: s.start_uptime_ms,
                    acked: 0,
                    sent: 0,
                    end_attempts: 0,
                    end_sent: false,
                    end_ack: false,
                });
            }
        }

        let Some(c) = ctx.as_mut() else {
            continue;
        };

        if Instant::now() < retry_at {
            continue;
        }

        if sock.is_none() {
            let peer = peers.get();
            let mut s = match connect_peer(peer.clone()) {
                Ok(s) => s,
                Err(e) => {
                    warn!("uploader: connect failed: {e}");
                    retry_at = Instant::now() + Duration::from_millis(backoff_ms);
                    warn!("uploader: retry in {} ms", backoff_ms);
                    backoff_ms = (backoff_ms * 2).min(30_000);
                    continue;
                }
            };
            if let Err(e) = send_start(&mut s, c, &device_id) {
                warn!("uploader: connect failed: {e}");
                retry_at = Instant::now() + Duration::from_millis(backoff_ms);
                warn!("uploader: retry in {} ms", backoff_ms);
                backoff_ms = (backoff_ms * 2).min(30_000);
                continue;
            }
            if let Some(peer) = &peer {
                info!("uploader: connected to {}:{}", peer.host, peer.port);
            }
            c.sent = c.acked;
            c.end_sent = false;
            c.end_ack = false;
            sock = Some(s);
            backoff_ms = 1_000;
        }

        let s = sock.as_mut().expect("socket connected above");
        let mut pcm = [0i16; 1600];
        loop {
            let Some(a) = shared.find_audio(c.id, c.sent, &mut pcm) else {
                break;
            };
            let header = protocol::header(protocol::AUDIO, (a.copy_len * 2) as u32);
            let bytes =
                unsafe { std::slice::from_raw_parts(pcm.as_ptr() as *const u8, a.copy_len * 2) };
            if let Err(e) = s.write_all(&header).and_then(|_| s.write_all(bytes)) {
                warn!("uploader: send failed: {e}");
                drop(sock.take());
                c.sent = c.acked;
                c.end_sent = false;
                c.end_ack = false;
                retry_at = Instant::now() + Duration::from_millis(backoff_ms);
                warn!("uploader: retry in {} ms", backoff_ms);
                backoff_ms = (backoff_ms * 2).min(30_000);
                break;
            }
            c.sent = a.first_sample + a.slot_len as u32;
        }

        if sock.is_none() {
            continue;
        }

        if let Some(e) = shared.find_end(c.id) {
            if c.sent >= e.total_samples && c.acked >= e.total_samples {
                if !c.end_sent {
                    let s = sock.as_mut().expect("socket connected above");
                    if let Err(err) = send_end(s, c, e.total_samples, e.reason) {
                        warn!("uploader: END send failed: {err}");
                        drop(sock.take());
                        retry_at = Instant::now() + Duration::from_millis(backoff_ms);
                        warn!("uploader: retry in {} ms", backoff_ms);
                        backoff_ms = (backoff_ms * 2).min(30_000);
                        continue;
                    }
                    c.end_sent = true;
                    c.end_ack = false;
                }

                let deadline = Instant::now() + Duration::from_millis(2_000);
                while !c.end_ack && Instant::now() < deadline {
                    let s = match sock.as_mut() {
                        Some(s) => s,
                        None => break,
                    };
                    if let Err(err) = drain_acks(s, &mut rx, &mut rx_len, c, &shared) {
                        warn!("uploader: connection lost: {err}");
                        drop(sock.take());
                        break;
                    }
                    if !c.end_ack {
                        FreeRtos::delay_ms(20);
                    }
                }

                if c.end_ack {
                    info!(
                        "uploader: session 0x{:08x} ended at {} samples",
                        c.id, e.total_samples
                    );
                    shared.pop_end(c.id, e.total_samples);
                    ctx = None;
                    sock = None;
                    backoff_ms = 1_000;
                    retry_at = Instant::now();
                } else {
                    drop(sock.take());
                    c.end_attempts += 1;
                    c.end_sent = false;
                    c.end_ack = false;
                    if c.end_attempts >= 3 {
                        error!(
                            "uploader: END not acknowledged for session 0x{:08x}; dropping",
                            c.id
                        );
                        shared.pop_end(c.id, e.total_samples);
                        ctx = None;
                        sock = None;
                        backoff_ms = 1_000;
                    } else {
                        retry_at = Instant::now() + Duration::from_millis(backoff_ms);
                        warn!("uploader: retry in {} ms", backoff_ms);
                        backoff_ms = (backoff_ms * 2).min(30_000);
                    }
                }
            }
        }
    }
}

fn connect_peer(peer: Option<Peer>) -> Result<TcpStream> {
    let peer = peer.ok_or_else(|| anyhow!("no receiver selected"))?;
    let mut addrs = (peer.host.as_str(), peer.port).to_socket_addrs()?;
    let addr = addrs.next().ok_or_else(|| anyhow!("no address for peer"))?;
    let s = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    s.set_nodelay(true)?;
    s.set_read_timeout(Some(Duration::from_millis(50)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(s)
}

fn send_start(s: &mut impl Write, c: &Ctx, device_id: &[u8; 16]) -> Result<()> {
    let start = protocol::Start {
        session_id: c.id,
        first_sample: c.acked,
        sample_rate: 16_000,
        channels: 1,
        bits: 16,
        start_unix_ms: 0,
        device_uptime_ms: c.start_uptime_ms as u64,
        device_id: *device_id,
    };
    s.write_all(&protocol::encode_start(&start))?;
    Ok(())
}

fn send_end(s: &mut impl Write, c: &Ctx, total_samples: u32, reason: u8) -> Result<()> {
    let end = protocol::End {
        session_id: c.id,
        total_samples,
        reason,
    };
    s.write_all(&protocol::encode_end(&end))?;
    Ok(())
}

fn drain_acks(
    s: &mut TcpStream,
    rx: &mut [u8; 64],
    rx_len: &mut usize,
    c: &mut Ctx,
    shared: &SharedQueue,
) -> Result<()> {
    let n = match s.read(&mut rx[*rx_len..]) {
        Ok(0) => bail!("peer closed connection"),
        Ok(n) => n,
        Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
            return Ok(())
        }
        Err(e) => return Err(e.into()),
    };
    *rx_len += n;

    while *rx_len >= protocol::HDR_LEN {
        let magic = u32::from_le_bytes(rx[0..4].try_into().unwrap());
        let version = rx[4];
        let typ = rx[5];
        let flags = u16::from_le_bytes([rx[6], rx[7]]);
        let len = u32::from_le_bytes(rx[8..12].try_into().unwrap()) as usize;
        if magic != protocol::MAGIC || version != protocol::VERSION {
            bail!("bad ACK header");
        }
        if typ != protocol::ACK || len != protocol::ACK_LEN {
            bail!("unexpected record type 0x{typ:02x} len {len}");
        }
        if *rx_len < protocol::HDR_LEN + len {
            break;
        }
        let ack = protocol::decode_ack(&rx[protocol::HDR_LEN..protocol::HDR_LEN + len])
            .ok_or_else(|| anyhow!("malformed ACK"))?;
        if ack.session_id == c.id {
            c.acked = c.acked.max(ack.acked_samples);
            shared.ack(c.id, c.acked);
            if flags & 1 != 0 {
                c.end_ack = true;
            }
        }
        rx.copy_within(protocol::HDR_LEN + len..*rx_len, 0);
        *rx_len -= protocol::HDR_LEN + len;
    }
    Ok(())
}
