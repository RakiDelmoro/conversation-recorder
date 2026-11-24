//! Safe wrapper around the vendored TEN VAD C implementation.
//!
//! TEN VAD is Copyright (c) 2025 Agora and licensed under the Apache
//! License 2.0 with additional conditions; see `src/ten_vad/LICENSE`.

use std::ffi::c_void;
use std::os::raw::c_int;

pub const HOP: usize = 256; // 16 ms at 16 kHz

extern "C" {
    fn ten_vad_create(handle: *mut *mut c_void, hop_size: usize, threshold: f32) -> c_int;
    fn ten_vad_process(
        handle: *mut c_void,
        audio_data: *const i16,
        audio_len: usize,
        out_probability: *mut f32,
        out_flag: *mut c_int,
    ) -> c_int;
    fn ten_vad_destroy(handle: *mut *mut c_void) -> c_int;
}

pub struct TenVad {
    handle: *mut c_void,
    frame: [i16; HOP],
    fill: usize,
    last_probability: f32,
}

unsafe impl Send for TenVad {}

impl TenVad {
    pub fn new(threshold: f32) -> Option<Self> {
        let mut handle: *mut c_void = std::ptr::null_mut();
        let rc = unsafe { ten_vad_create(&mut handle, HOP, threshold) };
        if rc != 0 || handle.is_null() {
            return None;
        }
        Some(Self {
            handle,
            frame: [0; HOP],
            fill: 0,
            last_probability: 0.0,
        })
    }

    /// Feed arbitrary 16 kHz mono PCM. Returns the most recent frame
    /// probability. Complete 256-sample frames are inferred in order.
    pub fn push(&mut self, pcm: &[i16]) -> f32 {
        for &sample in pcm {
            self.frame[self.fill] = sample;
            self.fill += 1;
            if self.fill == HOP {
                let mut probability = 0.0f32;
                let mut flag = 0i32;
                let rc = unsafe {
                    ten_vad_process(
                        self.handle,
                        self.frame.as_ptr(),
                        HOP,
                        &mut probability,
                        &mut flag,
                    )
                };
                if rc == 0 {
                    self.last_probability = probability;
                }
                self.fill = 0;
            }
        }
        self.last_probability
    }
}

impl Drop for TenVad {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                ten_vad_destroy(&mut self.handle);
            }
            self.handle = std::ptr::null_mut();
        }
    }
}
