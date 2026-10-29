//! 把麦克风的原生采样率（通常 44.1k/48k）降到 16kHz：先低通滤波防混叠，再线性插值。

pub struct Resampler {
    step: f64,
    taps: Vec<f32>,
    tail: Vec<f32>,
    pos: f64,
    prev: f32,
}

impl Resampler {
    pub fn new(in_rate: u32) -> Self {
        let step = in_rate as f64 / 16000.0;
        const N: usize = 31;
        let mid = (N / 2) as f64;
        let fc = 0.45 / step; // 截止频率约 7.2kHz（相对输入采样率的归一化频率）
        let mut taps: Vec<f32> = (0..N)
            .map(|i| {
                let x = i as f64 - mid;
                let sinc = if x == 0.0 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x) };
                let hamming = 0.54 - 0.46 * (2.0 * std::f64::consts::PI * i as f64 / (N - 1) as f64).cos();
                (sinc * hamming) as f32
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        Self { step, taps, tail: Vec::new(), pos: 0.0, prev: 0.0 }
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if (self.step - 1.0).abs() < 1e-9 {
            return input.to_vec();
        }
        let n = self.taps.len();
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(input);
        if buf.len() < n {
            self.tail = buf;
            return Vec::new();
        }
        let count = buf.len() - n + 1;
        let filtered: Vec<f32> = (0..count)
            .map(|i| buf[i..i + n].iter().zip(&self.taps).map(|(x, t)| x * t).sum())
            .collect();
        self.tail = buf[count..].to_vec();

        // 在序列 [prev, f0, f1, ...] 上按步长插值，pos 跨块连续
        let len = filtered.len() as f64;
        let mut out = Vec::with_capacity((len / self.step) as usize + 1);
        while self.pos < len {
            let i = self.pos.floor() as usize;
            let frac = (self.pos - i as f64) as f32;
            let a = if i == 0 { self.prev } else { filtered[i - 1] };
            let b = filtered[i];
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }
        self.pos -= len;
        self.prev = *filtered.last().unwrap();
        out
    }
}
