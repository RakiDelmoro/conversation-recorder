use std::mem::size_of;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail};

pub const CHUNK_SAMPLES: usize = 1600; // 100 ms at 16 kHz

#[derive(Clone, Copy)]
struct Slot {
    kind: u8, // 0 empty, 1 START, 2 AUDIO, 3 END
    reason: u8,
    len: u16, // samples
    session_id: u32,
    first_sample: u32,
    total_samples: u32,
    start_uptime_ms: u32,
    data: [i16; CHUNK_SAMPLES],
}

impl Slot {
    const fn empty() -> Self {
        Self {
            kind: 0,
            reason: 0,
            len: 0,
            session_id: 0,
            first_sample: 0,
            total_samples: 0,
            start_uptime_ms: 0,
            data: [0; CHUNK_SAMPLES],
        }
    }
}

pub struct StartInfo {
    pub session_id: u32,
    pub start_uptime_ms: u32,
}

pub struct EndInfo {
    pub total_samples: u32,
    pub reason: u8,
}

pub struct AudioChunk {
    pub first_sample: u32,
    pub slot_len: usize,
    pub copy_len: usize,
}

struct Queue {
    slots: Vec<Slot>,
    head: usize,
    len: usize,
}

pub struct SharedQueue {
    inner: Mutex<Queue>,
    work: Condvar,
}

impl SharedQueue {
    pub fn new(max_bytes: usize) -> anyhow::Result<Self> {
        let n = max_bytes / size_of::<Slot>();
        if n < 4 {
            bail!("queue too small for {} bytes", max_bytes);
        }

        let mut slots = Vec::new();
        slots
            .try_reserve_exact(n)
            .map_err(|_| anyhow!("PSRAM queue allocation failed"))?;
        slots.resize(n, Slot::empty());

        Ok(Self {
            inner: Mutex::new(Queue {
                slots,
                head: 0,
                len: 0,
            }),
            work: Condvar::new(),
        })
    }

    pub fn push_start(&self, session_id: u32, start_uptime_ms: u32) -> bool {
        let mut q = self.inner.lock().unwrap();
        let cap = q.slots.len();
        if q.len + 2 > cap {
            return false;
        }
        let tail = (q.head + q.len) % cap;
        q.slots[tail] = Slot {
            kind: 1,
            session_id,
            start_uptime_ms,
            ..Slot::empty()
        };
        q.len += 1;
        drop(q);
        self.work.notify_all();
        true
    }

    pub fn push_audio(&self, session_id: u32, first_sample: u32, pcm: &[i16]) -> bool {
        if pcm.len() > CHUNK_SAMPLES {
            return false;
        }
        let mut q = self.inner.lock().unwrap();
        let cap = q.slots.len();
        if q.len + 2 > cap {
            return false;
        }
        let tail = (q.head + q.len) % cap;
        let mut slot = Slot::empty();
        slot.kind = 2;
        slot.session_id = session_id;
        slot.first_sample = first_sample;
        slot.len = pcm.len() as u16;
        slot.data[..pcm.len()].copy_from_slice(pcm);
        q.slots[tail] = slot;
        q.len += 1;
        drop(q);
        self.work.notify_all();
        true
    }

    pub fn push_end(&self, session_id: u32, total_samples: u32, reason: u8) -> bool {
        let mut q = self.inner.lock().unwrap();
        let cap = q.slots.len();
        if q.len + 1 > cap {
            return false;
        }
        let tail = (q.head + q.len) % cap;
        q.slots[tail] = Slot {
            kind: 3,
            session_id,
            total_samples,
            reason,
            ..Slot::empty()
        };
        q.len += 1;
        drop(q);
        self.work.notify_all();
        true
    }

    pub fn take_start(&self) -> Option<StartInfo> {
        let mut q = self.inner.lock().unwrap();
        if q.len == 0 {
            return None;
        }
        let head = q.head;
        if q.slots[head].kind != 1 {
            return None;
        }
        let info = StartInfo {
            session_id: q.slots[head].session_id,
            start_uptime_ms: q.slots[head].start_uptime_ms,
        };
        q.slots[head].kind = 0;
        q.head = (q.head + 1) % q.slots.len();
        q.len -= 1;
        Some(info)
    }

    pub fn find_audio(
        &self,
        session_id: u32,
        offset: u32,
        out: &mut [i16],
    ) -> Option<AudioChunk> {
        let q = self.inner.lock().unwrap();
        let cap = q.slots.len();
        for i in 0..q.len {
            let idx = (q.head + i) % cap;
            let slot = &q.slots[idx];
            if slot.kind == 3 && slot.session_id == session_id {
                break;
            }
            if slot.kind == 2
                && slot.session_id == session_id
                && slot.first_sample.saturating_add(slot.len as u32) > offset
            {
                let skip = offset.saturating_sub(slot.first_sample) as usize;
                let len = slot.len as usize;
                if skip >= len || skip > out.len() {
                    continue;
                }
                let copy_len = (len - skip).min(out.len());
                out[..copy_len].copy_from_slice(&slot.data[skip..skip + copy_len]);
                return Some(AudioChunk {
                    first_sample: slot.first_sample,
                    slot_len: len,
                    copy_len,
                });
            }
        }
        None
    }

    pub fn find_end(&self, session_id: u32) -> Option<EndInfo> {
        let q = self.inner.lock().unwrap();
        let cap = q.slots.len();
        for i in 0..q.len {
            let idx = (q.head + i) % cap;
            let slot = &q.slots[idx];
            if slot.kind == 3 && slot.session_id == session_id {
                return Some(EndInfo {
                    total_samples: slot.total_samples,
                    reason: slot.reason,
                });
            }
        }
        None
    }

    pub fn ack(&self, session_id: u32, acked_samples: u32) {
        let mut q = self.inner.lock().unwrap();
        let cap = q.slots.len();
        while q.len > 0 {
            let head = q.head;
            let slot = &q.slots[head];
            if slot.kind == 2
                && slot.session_id == session_id
                && slot.first_sample.saturating_add(slot.len as u32) <= acked_samples
            {
                q.slots[head].kind = 0;
                q.head = (head + 1) % cap;
                q.len -= 1;
            } else {
                break;
            }
        }
    }

    pub fn pop_end(&self, session_id: u32, total_samples: u32) -> bool {
        let mut q = self.inner.lock().unwrap();
        if q.len == 0 {
            return false;
        }
        let head = q.head;
        let slot = &q.slots[head];
        if slot.kind == 3 && slot.session_id == session_id && slot.total_samples == total_samples {
            q.slots[head].kind = 0;
            q.head = (head + 1) % q.slots.len();
            q.len -= 1;
            true
        } else {
            false
        }
    }

    pub fn reset(&self) {
        let mut q = self.inner.lock().unwrap();
        q.head = 0;
        q.len = 0;
        drop(q);
        self.work.notify_all();
    }

    pub fn bytes(&self) -> usize {
        let q = self.inner.lock().unwrap();
        q.len * size_of::<Slot>()
    }

    pub fn wait(&self, timeout: Duration) {
        let guard = self.inner.lock().unwrap();
        let _ = self.work.wait_timeout(guard, timeout);
    }
}
