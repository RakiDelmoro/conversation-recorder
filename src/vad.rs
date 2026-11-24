pub struct EnergyVad {
    noise_dbfs: f32,
}

impl EnergyVad {
    pub fn new() -> Self {
        Self { noise_dbfs: -70.0 }
    }

    pub fn process(&mut self, pcm: &[i16]) -> bool {
        if pcm.is_empty() {
            return false;
        }

        let mut sum = 0.0f64;
        for &s in pcm {
            let v = s as f64;
            sum += v * v;
        }

        let rms = (sum / pcm.len() as f64).sqrt();
        let db = 20.0 * (rms / 32768.0).max(1e-9).log10() as f32;

        if db < self.noise_dbfs {
            self.noise_dbfs = 0.9 * self.noise_dbfs + 0.1 * db;
        } else {
            self.noise_dbfs = 0.995 * self.noise_dbfs + 0.005 * db;
        }
        self.noise_dbfs = self.noise_dbfs.clamp(-90.0, -20.0);

        db > self.noise_dbfs + 9.0 && db > -55.0
    }
}
