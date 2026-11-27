//! Adaptive recorder: speech-triggered, silence-ended utterances with a
//! hard 30 min cap for continuous speech.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use log::{error, info, warn};

use crate::config;
use crate::queue::SharedQueue;

const SAMPLE_RATE: u64 = 16_000;
const MAX_CHUNK: usize = 1600;
/// Two seconds of 20 ms audio blocks. The start gate uses the ratio of
/// speech blocks in this window, not consecutive speech.
const SPEECH_WINDOW_BLOCKS: usize = 100;
const CANDIDATE_BLOCK: usize = 320; // 20 ms at 16 kHz
const CANDIDATE_MAX_BLOCKS: usize = 150; // 3 s at 20 ms
const CANDIDATE_LONG_ACTIVE_MS: u32 = 1_500;

struct CandidateStats {
    active_ms: u32,
    zcr: f32,
    peaks: usize,
}

/// Rejects VADNet positives that are too quiet for the tracked room noise or
/// too short to be speech. This is the second stage after the model: VADNet
/// supplies the speech/non-speech decision, while this gate removes steady
/// noise and isolated clicks that the model can mistake for speech.
struct SpeechGate {
    noise_db: f32,
    power: f32,
    speech_ms: u32,
}

impl SpeechGate {
    fn new() -> Self {
        Self {
            noise_db: -60.0,
            power: 1.0e-6, // -60 dBFS in normalized mean-square power
            speech_ms: 0,
        }
    }

    fn update(&mut self, pcm: &[i16], vad_speech: bool) -> bool {
        if pcm.is_empty() {
            self.speech_ms = 0;
            return false;
        }

        let mut sum = 0.0f32;
        for &sample in pcm {
            let value = sample as f32 / 32768.0;
            sum += value * value;
        }
        let power = sum / pcm.len() as f32;
        self.power += (power - self.power) * config::SPEECH_GATE_SMOOTHING;
        let db = 10.0 * self.power.max(1.0e-18).log10();
        let threshold =
            (self.noise_db + config::SPEECH_GATE_MARGIN_DB).max(config::SPEECH_GATE_MIN_DBFS);

        // Only non-speech, sub-threshold audio feeds the noise estimate. A
        // click above the threshold must not drag the floor upward.
        if !vad_speech && db < threshold {
            self.noise_db +=
                (db - self.noise_db) * config::SPEECH_GATE_NOISE_ALPHA;
            self.noise_db = self.noise_db.clamp(-80.0, -20.0);
        }

        let block_ms = (pcm.len() as u32 * 1000 / SAMPLE_RATE as u32).max(1);
        if vad_speech && db >= threshold {
            self.speech_ms = self
                .speech_ms
                .saturating_add(block_ms)
                .min(1_000);
        } else {
            // Leaky release: a short gap does not immediately drop a confirmed
            // speech run, but isolated clicks never accumulate enough.
            self.speech_ms = self.speech_ms.saturating_sub(block_ms * 2);
        }

        vad_speech && self.speech_ms >= config::SPEECH_GATE_CONFIRM_MS
    }
}

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
    speech_hist: [bool; SPEECH_WINDOW_BLOCKS],
    speech_hist_pos: usize,
    speech_hist_count: usize,
    silence_run_ms: u32,
    ring_silence_ms: u32,
    active_speech_ms: u32,
    emit_open: bool,
    pre_roll: Vec<i16>,
    pre_roll_cap: usize,
    pre_roll_len: usize,
    pre_roll_pos: usize,
    pending: [i16; MAX_CHUNK],
    pending_len: usize,
    queue_failed: bool,
    queue_error_logged_ms: i64,
    cooldown_until_ms: i64,
    one_shot_started: bool,
    one_shot_done: bool,
    retry_at_ms: u64,
    acked: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    recording: Arc<AtomicBool>,
    ring_speech: Arc<AtomicBool>,
    speech_gate: SpeechGate,
}

impl Recorder {
    pub fn new(
        recording: Arc<AtomicBool>,
        acked: Arc<AtomicBool>,
        restart: Arc<AtomicBool>,
        ring_speech: Arc<AtomicBool>,
    ) -> anyhow::Result<Box<Self>> {
        let cap = config::PRE_ROLL_MS as usize * 16;
        let mut pre_roll = Vec::new();
        let pre_roll_cap = if pre_roll.try_reserve_exact(cap).is_ok() {
            pre_roll.resize(cap, 0);
            cap
        } else {
            error!("recorder: pre-roll allocation failed");
            0
        };

        if config::LOG_ONLY {
            info!("recorder: LOG_ONLY enabled; candidates are logged but never queued or sent");
        }

        Ok(Box::new(Self {
            state: State::Idle,
            input_samples: 0,
            session_id: 0,
            total_samples: 0,
            window_start_ms: 0,
            speech_run_ms: 0,
            speech_hist: [false; SPEECH_WINDOW_BLOCKS],
            speech_hist_pos: 0,
            speech_hist_count: 0,
            silence_run_ms: 0,
            ring_silence_ms: 0,
            active_speech_ms: 0,
            emit_open: false,
            pre_roll,
            pre_roll_cap,
            pre_roll_len: 0,
            pre_roll_pos: 0,
            pending: [0; MAX_CHUNK],
            pending_len: 0,
            queue_failed: false,
            queue_error_logged_ms: i64::MIN / 2,
            cooldown_until_ms: 0,
            one_shot_started: false,
            one_shot_done: false,
            retry_at_ms: 0,
            acked,
            restart,
            recording,
            ring_speech,
            speech_gate: SpeechGate::new(),
        }))
    }

    fn publish_led(&self) {
        let recording = matches!(self.state, State::Active);
        self.recording.store(recording, Ordering::Relaxed);
        self.ring_speech.store(
            recording && self.ring_silence_ms < config::RING_SPEECH_HOLD_MS,
            Ordering::Relaxed,
        );
    }

    pub fn process(&mut self, pcm: &[i16], shared: &SharedQueue, speech: bool) {
        if config::ONE_SHOT {
            self.process_one_shot(pcm, shared);
            return;
        }

        self.push_pre_roll(pcm);
        let speech = self.speech_gate.update(pcm, speech);

        let block_ms = (pcm.len() as u32 * 1000 / SAMPLE_RATE as u32).max(1);
        let now_ms = (self.input_samples as u64 + pcm.len() as u64) * 1000 / SAMPLE_RATE;

        if matches!(self.state, State::Idle) {
            let idx = self.speech_hist_pos;
            let was = self.speech_hist[idx];
            if speech && !was {
                self.speech_hist_count += 1;
            } else if !speech && was {
                self.speech_hist_count -= 1;
            }
            self.speech_hist[idx] = speech;
            self.speech_hist_pos = (idx + 1) % SPEECH_WINDOW_BLOCKS;
            self.speech_run_ms = self.speech_hist_count as u32 * block_ms;

            if speech
                && self.speech_run_ms >= config::MIN_SPEECH_MS
                && now_ms as i64 >= self.cooldown_until_ms
            {
                let stats = self.candidate_stats();
                let accepted = stats.active_ms >= config::CANDIDATE_MIN_ACTIVE_MS
                    && (stats.peaks >= 2 || stats.active_ms >= CANDIDATE_LONG_ACTIVE_MS);
                info!(
                    "recorder: candidate check active={} ms, zcr={:.2}, peaks={}, run={} ms",
                    stats.active_ms, stats.zcr, stats.peaks, self.speech_run_ms
                );
                if !accepted {
                    warn!(
                        "recorder: candidate rejected (active={} ms, zcr={:.2}, peaks={})",
                        stats.active_ms, stats.zcr, stats.peaks
                    );
                    self.reset_speech_window();
                    self.cooldown_until_ms = now_ms as i64 + 1000;
                    self.input_samples += pcm.len() as u64;
                    self.publish_led();
                    return;
                }

                if config::LOG_ONLY {
                    info!(
                        "recorder: candidate accepted (log-only) active={} ms, zcr={:.2}, peaks={}, run={} ms",
                        stats.active_ms, stats.zcr, stats.peaks, self.speech_run_ms
                    );
                    self.log_simulated_session(stats.active_ms);
                    self.reset_speech_window();
                    self.cooldown_until_ms = now_ms as i64 + 1000;
                    self.input_samples += pcm.len() as u64;
                    self.publish_led();
                    return;
                }

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
                    self.ring_silence_ms = 0;
                    self.active_speech_ms = 0;
                    self.emit_open = true;
                    self.pending_len = 0;
                    self.queue_failed = false;

                    if !self.emit_pre_roll(shared) || self.queue_failed {
                        self.recorder_end(1, shared);
                    } else {
                        info!("session 0x{:08x} started", self.session_id);
                    }
                } else if now_ms as i64 - self.queue_error_logged_ms >= 1000 {
                    error!("recorder: cannot queue session start");
                    self.queue_error_logged_ms = now_ms as i64;
                    self.reset_speech_window();
                    self.cooldown_until_ms = now_ms as i64 + 1000;
                }

                self.input_samples += pcm.len() as u64;
                self.publish_led();
                return;
            }

            self.input_samples += pcm.len() as u64;
            self.publish_led();
            return;
        }

        if speech {
            self.ring_silence_ms = 0;
            self.active_speech_ms = self.active_speech_ms.saturating_add(block_ms);
            if self.active_speech_ms >= config::SPEECH_RESUME_MS {
                self.silence_run_ms = 0;
                self.emit_open = true;
                self.active_speech_ms = config::SPEECH_RESUME_MS;
            }
        } else {
            self.active_speech_ms = 0;
            self.silence_run_ms = self.silence_run_ms.saturating_add(block_ms);
            self.ring_silence_ms = self.ring_silence_ms.saturating_add(block_ms);
        }

        if self.silence_run_ms <= config::HOLDBACK_MS {
            if !self.emit_audio(pcm, shared) {
                self.recorder_end(1, shared);
                self.input_samples += pcm.len() as u64;
                self.publish_led();
                return;
            }
        } else if self.emit_open {
            let flushed = self.flush_pending(shared);
            self.emit_open = false;
            if !flushed {
                self.recorder_end(1, shared);
                self.input_samples += pcm.len() as u64;
                self.publish_led();
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
        self.publish_led();
    }

    fn process_one_shot(&mut self, pcm: &[i16], shared: &SharedQueue) {
        self.ring_speech.store(false, Ordering::Relaxed);
        let len = pcm.len() as u64;
        let now_ms = (self.input_samples + len) * 1000 / SAMPLE_RATE;

        if self.one_shot_done {
            if self.acked.load(Ordering::Relaxed) {
                self.input_samples += len;
                self.recording.store(false, Ordering::Relaxed);
                return;
            }
            if now_ms >= self.retry_at_ms {
                warn!(
                    "recorder: no ACK within {} ms; discarding and restarting",
                    config::ONE_SHOT_ACK_TIMEOUT_MS
                );
                shared.reset();
                self.restart.store(true, Ordering::Relaxed);
                self.one_shot_started = false;
                self.one_shot_done = false;
                self.retry_at_ms = 0;
                self.state = State::Idle;
                self.pending_len = 0;
                self.emit_open = false;
                self.total_samples = 0;
                self.queue_failed = false;
                self.acked.store(false, Ordering::Relaxed);
                self.input_samples = 0;
                self.recording.store(false, Ordering::Relaxed);
                return;
            }
            self.input_samples += len;
            self.recording.store(false, Ordering::Relaxed);
            return;
        }

        if !self.one_shot_started {
            let mut id = unsafe { esp_idf_svc::sys::esp_random() };
            if id == 0 {
                id = 1;
            }
            let start_ms = (unsafe { esp_idf_svc::sys::esp_timer_get_time() } / 1000) as u32;
            if !shared.push_start(id, start_ms) {
                if now_ms as i64 - self.queue_error_logged_ms >= 1000 {
                    error!("recorder: one-shot cannot queue start");
                    self.queue_error_logged_ms = now_ms as i64;
                }
                self.input_samples += len;
                return;
            }
            self.state = State::Active;
            self.session_id = id;
            self.total_samples = 0;
            self.window_start_ms = now_ms;
            self.pending_len = 0;
            self.queue_failed = false;
            self.emit_open = true;
            self.one_shot_started = true;
            self.acked.store(false, Ordering::Relaxed);
            self.recording.store(true, Ordering::Relaxed);
            info!(
                "recorder: one-shot session 0x{:08x} started ({} ms)",
                id, config::ONE_SHOT_MS
            );
        }

        if !self.emit_audio(pcm, shared) {
            self.recorder_end(1, shared);
            self.one_shot_done = true;
            self.retry_at_ms = now_ms + config::ONE_SHOT_ACK_TIMEOUT_MS;
            self.input_samples += len;
            self.recording.store(false, Ordering::Relaxed);
            return;
        }

        if now_ms.saturating_sub(self.window_start_ms) >= config::ONE_SHOT_MS {
            self.recorder_end(0, shared);
            self.one_shot_done = true;
            self.retry_at_ms = now_ms + config::ONE_SHOT_ACK_TIMEOUT_MS;
            info!("recorder: one-shot session complete");
        }

        self.input_samples += len;
        self.recording.store(!self.one_shot_done, Ordering::Relaxed);
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

    fn reset_speech_window(&mut self) {
        self.speech_hist.fill(false);
        self.speech_hist_pos = 0;
        self.speech_hist_count = 0;
        self.speech_run_ms = 0;
    }

    fn candidate_stats(&self) -> CandidateStats {
        let cap = self.pre_roll_cap;
        let total = self.pre_roll_len;
        if cap == 0 || total < CANDIDATE_BLOCK {
            return CandidateStats {
                active_ms: 0,
                zcr: 1.0,
                peaks: 0,
            };
        }

        let start = if total < cap { 0 } else { self.pre_roll_pos };
        let blocks = (total / CANDIDATE_BLOCK).min(CANDIDATE_MAX_BLOCKS);
        let mut rms = [0.0f32; CANDIDATE_MAX_BLOCKS];
        let mut zcr = [0.0f32; CANDIDATE_MAX_BLOCKS];

        for b in 0..blocks {
            let base = start + b * CANDIDATE_BLOCK;
            let mut sum = 0.0f64;
            let mut crossings = 0u32;
            let mut prev = 0i16;
            for j in 0..CANDIDATE_BLOCK {
                let s = self.pre_roll[(base + j) % cap];
                let v = s as f64;
                sum += v * v;
                if j > 0 && ((s >= 0) != (prev >= 0)) {
                    crossings += 1;
                }
                prev = s;
            }
            rms[b] = (sum / CANDIDATE_BLOCK as f64).sqrt() as f32;
            zcr[b] = crossings as f32 / CANDIDATE_BLOCK as f32;
        }

        let mut sorted = rms;
        sorted[..blocks].sort_unstable_by(|a, b| {
            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
        });
        let floor = sorted[(blocks / 5).min(blocks - 1)];
        let active_threshold = (floor * 3.0).max(30.0);

        let mut active = 0u32;
        let mut zcr_sum = 0.0f32;
        let mut peaks = 0usize;
        let mut above = false;
        for b in 0..blocks {
            let is_active = rms[b] > active_threshold;
            if is_active {
                active += 1;
                zcr_sum += zcr[b];
            }
            if is_active && !above {
                peaks += 1;
            }
            above = is_active;
        }

        let active_ms = active * (CANDIDATE_BLOCK as u32 * 1000 / SAMPLE_RATE as u32);
        let avg_zcr = if active > 0 {
            zcr_sum / active as f32
        } else {
            1.0
        };
        CandidateStats {
            active_ms,
            zcr: avg_zcr,
            peaks,
        }
    }

    fn log_simulated_session(&self, active_ms: u32) {
        let mut id = unsafe { esp_idf_svc::sys::esp_random() };
        if id == 0 {
            id = 1;
        }
        let pre_roll_samples = self.pre_roll_len as u32;
        let candidate_samples = active_ms.saturating_mul(16);
        let total = pre_roll_samples.saturating_add(candidate_samples);
        info!(
            "sim: START session=0x{:08x} first=0 rate=16000 ch=1 bits=16 pre_roll={} ms",
            id,
            pre_roll_samples as u64 * 1000 / SAMPLE_RATE
        );
        info!(
            "sim: AUDIO session=0x{:08x} first=0 samples={} bytes={}",
            id,
            pre_roll_samples,
            pre_roll_samples * 2
        );
        info!(
            "sim: AUDIO session=0x{:08x} first={} samples={} bytes={}",
            id,
            pre_roll_samples,
            candidate_samples,
            candidate_samples * 2
        );
        info!(
            "sim: END session=0x{:08x} total={} reason=0",
            id,
            total
        );
        info!(
            "sim: ACK session=0x{:08x} acked={} final=true",
            id,
            total
        );
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
            self.publish_led();
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
            self.ring_silence_ms = 0;
            self.publish_led();
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
        self.ring_silence_ms = 0;
        self.cooldown_until_ms = now_ms as i64 + 1000;
        self.publish_led();
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

        self.reset_speech_window();
        self.silence_run_ms = 0;
        self.ring_silence_ms = 0;
        self.active_speech_ms = 0;
        self.pending_len = 0;
        self.queue_failed = false;
        self.emit_open = false;
        self.cooldown_until_ms = (self.input_samples * 1000 / SAMPLE_RATE) as i64 + 1000;
        self.publish_led();
        self.clear_pre_roll();
    }
}
