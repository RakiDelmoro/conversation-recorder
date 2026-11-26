//! Rust wrapper around the ESP-SR VADNet C bridge.

use std::os::raw::c_int;

extern "C" {
    fn vadnet_bridge_init() -> c_int;
    fn vadnet_bridge_push(samples: *const i16, count: c_int) -> c_int;
    fn vadnet_bridge_destroy();
}

pub struct VadNet {
    _private: (),
}

impl VadNet {
    pub fn new() -> Option<Self> {
        let rc = unsafe { vadnet_bridge_init() };
        if rc == 0 {
            Some(Self { _private: () })
        } else {
            None
        }
    }

    pub fn push(&mut self, pcm: &[i16]) -> bool {
        if pcm.is_empty() {
            return true;
        }
        unsafe { vadnet_bridge_push(pcm.as_ptr(), pcm.len() as c_int) != 0 }
    }
}

impl Drop for VadNet {
    fn drop(&mut self) {
        unsafe { vadnet_bridge_destroy() };
    }
}
