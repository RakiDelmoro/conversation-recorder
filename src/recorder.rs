//! Adaptive recorder: speech-triggered, silence-ended utterances with a
//! hard 60 s cap for continuous speech.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use log::{error, info, warn};

use crate::config;
use crate::queue::SharedQueue;
use crate::vad::EnergyVad;
use crate::xvf3800::VadState;

const SAMPLE_RATE: u64 = 16_000;
const MAX_CHUNK: usize = 1600;

enum State {
    Idle,
    Active,
}

pub struct Recorder {
    state: State,
    input_samples: u64,
    session_id: u32,
    total_samples: u32,
    window_start_ms: u64,
    speech_run_ms: u32,
    silence_run_ms: u32,
    emit_open: bool,
    pre_roll: Vec<i16>,
    pre_roll_cap: usize,
    pre_roll_len: usize,
    pre_roll_pos: usize,
    pending: [i16; MAX_CHUNK],
    pending_len: usize,
    energy: EnergyVad,
    use_model: bool,
    degraded: bool,
    queue_failed: bool,
    queue_error_logged_ms: i64,
    cooldown_until_ms: i64,
    recording: Arc<AtomicBool>,
}

impl Recorder {
    pub fn new(recording: Arc<AtomicBool>, use_model: bool) -> anyhow::Result<Box<Self>> {
        let cap = config::PRE_ROLL_MS as usize * 16;
        let mut pre_roll = Vec::new();
        let pre_roll_cap = if pre_roll.try_reserve_exact(cap).is_ok() {
            pre_roll.resize(cap, 0);
            cap
        } else {
            error!("recorder: pre-roll allocation failed");
            0
        };

        Ok(Box::new(Self {
            state: State::Idle,
            input_samples: 0,
            session_id: 0,
            total_samples: 0,
            window_start_ms: 0,
            speech_run_ms: 0,
            silence_run_ms: 0,
            emit_open: false,
            pre_roll,
            pre_roll_cap,
            pre_roll_len: 0,
            pre_roll_pos: 0,
            pending: [0; MAX_CHUNK],
            pending_len: 0,
            energy: EnergyVad::new(),
            use_model,
            degraded: false,
            queue_failed: false,
            queue_error_logged_ms: i64::MIN / 2,
            cooldown_until_ms: 0,
            recording,
        }))
    }

    pub fn process(
        &mut self,
        pcm: &[i16],
        vad: &VadState,
        shared: &SharedQueue,
        model_probability: f32,
    ) {
        let snap = vad.snapshot();
        let speech = if self.use_model {
            model_probability >= config::TEN_VAD_THRESHOLD
        } else if snap.healthy {
            snap.speech
        } else {
            self.energy.process(pcm)
        };

        if !self.use_model {
            if !snap.healthy && !self.degraded {
                warn!("XVF3800 I2C unhealthy; using energy VAD fallback");
                self.degraded = true;
            } else if snap.healthy && self.degraded {
                info!("XVF3800 I2C recovered");
                self.degraded = false;
            }
        }

        self.push_pre_roll(pcm);

        let block_ms = (pcm.len() as u32 * 1000 / SAMPLE_RATE as u32).max(1);
        let now_ms = (self.input_samples as u64 + pcm.len() as u64) * 1000 / SAMPLE_RATE;

        if matches!(self.state, State::Idle) {
            if speech {
                self.speech_run_ms = self.speech_run_ms.saturating_add(block_ms);
            } else {
                self.speech_run_ms = 0;
            }

            if speech
                && self.speech_run_ms >= config::MIN_SPEECH_MS
                && now_ms as i64 >= self.cooldown_until_ms
            {
                let mut id = unsafe { esp_idf_svc::sys::esp_random() };
                if id == 0 {
                    id = 1;
                }
                self.session_id = id;
                let start_ms = (unsafe { esp_idf_svc::sys::esp_timer_get_time() } / 1000) as u32;

                if shared.push_start(self.session_id, start_ms) {
                    self.state = State::Active;
                    self.total_samples = 0;
                    let pre_roll_ms = self.pre_roll_len as u64 * 1000 / SAMPLE_RATE;
                    self.window_start_ms = now_ms.saturating_sub(pre_roll_ms);
                    self.silence_run_ms = 0;
                    self.emit_open = true;
                    self.pending_len = 0;
                    self.queue_failed = false;

                    if !self.emit_pre_roll(shared) || self.queue_failed {
                        self.recorder_end(1, shared);
                    } else {
                        info!(
                            "session 0x{:08x} started (doa={})",
                            self.session_id, snap.doa
                        );
                    }
                } else if now_ms as i64 - self.queue_error_logged_ms >= 1000 {
                    error!("recorder: cannot queue session start");
                    self.queue_error_logged_ms = now_ms as i64;
                    self.speech_run_ms = 0;
                    self.cooldown_until_ms = now_ms as i64 + 1000;
                }

                self.input_samples += pcm.len() as u64;
                self.recording.store(false, Ordering::Relaxed);
                return;
            }

            self.input_samples += pcm.len() as u64;
            self.recording.store(false, Ordering::Relaxed);
            return;
        }

        if speech {
            self.silence_run_ms = 0;
            self.emit_open = true;
        } else {
            self.silence_run_ms = self.silence_run_ms.saturating_add(block_ms);
        }

        if self.silence_run_ms <= config::HOLDBACK_MS {
            if !self.emit_audio(pcm, shared) {
                self.recorder_end(1, shared);
                self.input_samples += pcm.len() as u64;
                self.recording.store(false, Ordering::Relaxed);
                return;
            }
        } else if self.emit_open {
            let flushed = self.flush_pending(shared);
            self.emit_open = false;
            if !flushed {
                self.recorder_end(1, shared);
                self.input_samples += pcm.len() as u64;
                self.recording.store(false, Ordering::Relaxed);
                return;
            }
        }

        if matches!(self.state, State::Active) {
            if self.silence_run_ms as u64 >= config::SILENCE_TIMEOUT_MS {
                self.recorder_end(0, shared);
            } else if now_ms.saturating_sub(self.window_start_ms) >= config::SEGMENT_MS {
                if self.emit_open {
                    self.rotate_segment(now_ms, shared);
                } else {
                    self.recorder_end(2, shared);
                }
            }
        }

        self.input_samples += pcm.len() as u64;
        self.recording
            .store(matches!(self.state, State::Active), Ordering::Relaxed);
    }

    fn push_pre_roll(&mut self, pcm: &[i16]) {
        let cap = self.pre_roll_cap;
        if cap == 0 {
            return;
        }
        for &s in pcm {
            if self.pre_roll_len < cap {
                let idx = (self.pre_roll_pos + self.pre_roll_len) % cap;
                self.pre_roll[idx] = s;
                self.pre_roll_len += 1;
            } else {
                self.pre_roll[self.pre_roll_pos] = s;
                self.pre_roll_pos = (self.pre_roll_pos + 1) % cap;
            }
        }
    }

    fn clear_pre_roll(&mut self) {
        self.pre_roll_len = 0;
        self.pre_roll_pos = 0;
    }

    fn emit_pre_roll(&mut self, shared: &SharedQueue) -> bool {
        let cap = self.pre_roll_cap;
        if cap == 0 || self.pre_roll_len == 0 {
            return !self.queue_failed;
        }
        let total = self.pre_roll_len;
        let start = if total < cap { 0 } else { self.pre_roll_pos };
        let mut tmp = [0i16; MAX_CHUNK];
        let mut emitted = 0usize;
        while emitted < total {
            let idx = (start + emitted) % cap;
            let n = (total - emitted).min(MAX_CHUNK).min(cap - idx);
            tmp[..n].copy_from_slice(&self.pre_roll[idx..idx + n]);
            if !self.emit_audio(&tmp[..n], shared) {
                return false;
            }
            emitted += n;
        }
        true
    }

    fn emit_audio(&mut self, pcm: &[i16], shared: &SharedQueue) -> bool {
        let mut offset = 0usize;
        while offset < pcm.len() {
            let space = MAX_CHUNK - self.pending_len;
            let n = space.min(pcm.len() - offset);
            self.pending[self.pending_len..self.pending_len + n]
                .copy_from_slice(&pcm[offset..offset + n]);
            self.pending_len += n;
            offset += n;
            if self.pending_len == MAX_CHUNK && !self.flush_pending(shared) {
                return false;
            }
        }
        true
    }

    fn flush_pending(&mut self, shared: &SharedQueue) -> bool {
        if self.pending_len == 0 {
            return true;
        }
        if shared.push_audio(self.session_id, self.total_samples, &self.pending[..self.pending_len]) {
            self.total_samples += self.pending_len as u32;
            self.pending_len = 0;
            true
        } else {
            let now_ms = self.input_samples as i64 * 1000 / SAMPLE_RATE as i64;
            if now_ms - self.queue_error_logged_ms >= 1000 {
                error!("recorder: queue full: session 0x{:08x}", self.session_id);
                self.queue_error_logged_ms = now_ms;
            }
            self.pending_len = 0;
            self.queue_failed = true;
            false
        }
    }

    fn rotate_segment(&mut self, now_ms: u64, shared: &SharedQueue) {
        if !matches!(self.state, State::Active) {
            return;
        }
        if !self.flush_pending(shared) {
            self.recorder_end(1, shared);
            return;
        }

        let old_id = self.session_id;
        let old_total = self.total_samples;
        if !shared.push_end(old_id, old_total, 2) {
            error!("recorder: cannot queue segment end");
            self.state = State::Idle;
            self.recording.store(false, Ordering::Relaxed);
            self.cooldown_until_ms = now_ms as i64 + 1000;
            self.clear_pre_roll();
            return;
        }

        let mut id = unsafe { esp_idf_svc::sys::esp_random() };
        if id == 0 {
            id = 1;
        }
        let start_ms = (unsafe { esp_idf_svc::sys::esp_timer_get_time() } / 1000) as u32;
        if shared.push_start(id, start_ms) {
            self.session_id = id;
            self.total_samples = 0;
            self.window_start_ms = now_ms;
            self.pending_len = 0;
            self.emit_open = true;
            self.queue_failed = false;
            self.recording.store(true, Ordering::Relaxed);
            info!(
                "recorder: segment 0x{:08x} ended at {} samples; next 0x{:08x}",
                old_id, old_total, self.session_id
            );
            return;
        }

        error!("recorder: cannot queue next segment start");
        self.state = State::Idle;
        self.pending_len = 0;
        self.queue_failed = false;
        self.emit_open = false;
        self.silence_run_ms = 0;
        self.cooldown_until_ms = now_ms as i64 + 1000;
        self.recording.store(false, Ordering::Relaxed);
        self.clear_pre_roll();
    }

    fn recorder_end(&mut self, mut reason: u8, shared: &SharedQueue) {
        if !matches!(self.state, State::Active) {
            return;
        }
        self.state = State::Idle;

        if reason == 0 && !self.queue_failed && !self.flush_pending(shared) {
            reason = 1;
        }
        if reason != 0 || self.pending_len > 0 {
            warn!("recorder: dropping {} pending samples", self.pending_len);
        }

        if !shared.push_end(self.session_id, self.total_samples, reason) {
            error!("recorder: cannot queue session end");
        }
        info!(
            "recorder: session 0x{:08x} ended, {} samples ({} ms), reason {}",
            self.session_id,
            self.total_samples,
            self.total_samples as u64 * 1000 / SAMPLE_RATE,
            reason
        );

        self.speech_run_ms = 0;
        self.silence_run_ms = 0;
        self.pending_len = 0;
        self.queue_failed = false;
        self.emit_open = false;
        self.cooldown_until_ms = (self.input_samples * 1000 / SAMPLE_RATE) as i64 + 1000;
        self.recording.store(false, Ordering::Relaxed);
        self.clear_pre_roll();
    }
}
