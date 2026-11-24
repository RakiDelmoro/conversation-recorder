//! UDP discovery: the device advertises itself and a receiver offers its
//! TCP address. The uploader connects out to the selected receiver.

use std::net::{Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use esp_idf_hal::delay::FreeRtos;
use log::{info, warn};

use crate::config;
use crate::queue::SharedQueue;

const BEACON_INTERVAL: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub host: String,
    pub port: u16,
}

#[derive(Default)]
pub struct PeerState {
    inner: Mutex<Option<Peer>>,
}

impl PeerState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    pub fn set(&self, peer: Peer) {
        *self.inner.lock().unwrap() = Some(peer);
    }

    pub fn get(&self) -> Option<Peer> {
        self.inner.lock().unwrap().clone()
    }
}

pub fn device_id_bytes() -> [u8; 16] {
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

pub fn device_id_hex() -> String {
    let id = device_id_bytes();
    String::from_utf8_lossy(&id[..12]).into_owned()
}

fn now_ms() -> u64 {
    (unsafe { esp_idf_svc::sys::esp_timer_get_time() } / 1000) as u64
}

fn parse_offer(line: &str, device_id: &str) -> Option<Peer> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 5 || parts[0] != "CREC/1" || parts[1] != "OFFER" {
        return None;
    }
    if parts[2] != device_id {
        return None;
    }
    let host: Ipv4Addr = parts[3].parse().ok()?;
    let port: u16 = parts[4].parse().ok()?;
    let token = parts.get(5).copied().unwrap_or("");
    if token != config::PAIRING_TOKEN {
        warn!("discovery: offer rejected: token mismatch");
        return None;
    }
    Some(Peer {
        host: host.to_string(),
        port,
    })
}

pub fn task(
    shared: Arc<SharedQueue>,
    recording: Arc<AtomicBool>,
    peers: Arc<PeerState>,
) -> Result<()> {
    if config::PAIRING_TOKEN.is_empty() {
        anyhow::bail!("PAIRING_TOKEN is empty; discovery disabled");
    }
    if config::PAIRING_TOKEN.chars().any(|c| c.is_whitespace()) {
        anyhow::bail!("PAIRING_TOKEN must not contain whitespace");
    }

    let socket = UdpSocket::bind(("0.0.0.0", config::DISCOVERY_PORT))
        .with_context(|| format!("bind UDP {}", config::DISCOVERY_PORT))?;
    socket.set_broadcast(true)?;
    socket.set_read_timeout(Some(READ_TIMEOUT))?;

    let device_id = device_id_hex();
    info!(
        "discovery: advertising as {} on UDP {}",
        device_id,
        config::DISCOVERY_PORT
    );

    let mut last_beacon: Option<Instant> = None;
    let mut last_send_error: Option<Instant> = None;
    let mut buf = [0u8; 512];

    loop {
        if last_beacon.map_or(true, |t| t.elapsed() >= BEACON_INTERVAL) {
            let state = if recording.load(Ordering::Relaxed) {
                "rec"
            } else {
                "idle"
            };
            let msg = format!(
                "CREC/1 BEACON {} {} {} {}\n",
                device_id,
                state,
                shared.bytes(),
                now_ms()
            );
            if let Err(e) = socket.send_to(
                msg.as_bytes(),
                ("255.255.255.255", config::DISCOVERY_PORT),
            ) {
                if last_send_error
                    .map_or(true, |t| t.elapsed() >= Duration::from_secs(30))
                {
                    warn!("discovery: beacon send failed: {e}");
                    last_send_error = Some(Instant::now());
                }
            }
            last_beacon = Some(Instant::now());
        }

        match socket.recv_from(&mut buf) {
            Ok((n, _src)) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                if let Some(peer) = parse_offer(text.trim(), &device_id) {
                    let selected = peer.clone();
                    peers.set(peer);
                    info!(
                        "discovery: receiver {}:{} selected",
                        selected.host, selected.port
                    );
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                warn!("discovery: receive failed: {e}");
                FreeRtos::delay_ms(100);
            }
        }
    }
}
