//! 流式重采样：加窗 sinc 插值（Blackman 窗，每侧 16 个过零点）。
//! 延迟只有半个滤波器长：48k→16k 约 1 ms，24k→48k 约 0.4 ms。

use std::f64::consts::PI;

const ZEROS: f64 = 16.0; // 每侧过零点数
const RES: usize = 256; // 每个输入采样间隔的查表精度
const PASS: f64 = 0.92; // 通带占目标奈奎斯特频率的比例

pub struct Resampler {
    step: f64, // 每个输出采样前进多少个输入采样
    half: usize,
    scale: f64, // 查表下标 = |t| * scale
    table: Vec<f32>,
    buf: Vec<f32>,
    pos: f64, // 下一个输出采样在 buf 里的位置
    passthrough: bool,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let step = from as f64 / to as f64;
        let cutoff = (to as f64 / from as f64).min(1.0) * PASS; // 相对输入奈奎斯特
        let half = (ZEROS / cutoff).ceil() as usize;
        let n = half * RES;
        let table = (0..=n)
            .map(|i| {
                let t = i as f64 / RES as f64; // 以输入采样为单位
                let x = cutoff * t;
                let sinc = if x == 0.0 { 1.0 } else { (PI * x).sin() / (PI * x) };
                let w = t / half as f64; // 0..1
                let win = 0.42 + 0.5 * (PI * w).cos() + 0.08 * (2.0 * PI * w).cos();
                (cutoff * sinc * win) as f32
            })
            .collect();
        Self {
            step,
            half,
            scale: RES as f64,
            table,
            buf: vec![0.0; half], // 左侧补零作为历史
            pos: half as f64,
            passthrough: from == to,
        }
    }

    /// 送入一段输入，把能算出的输出追加到 `out`。
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        self.buf.extend_from_slice(input);
        let h = self.half as f64;
        let last = self.table.len() - 1;
        while self.pos + h < self.buf.len() as f64 {
            let center = self.pos.floor() as isize;
            let lo = (center - self.half as isize + 1).max(0) as usize;
            let hi = ((center + self.half as isize) as usize).min(self.buf.len() - 1);
            let mut acc = 0.0f32;
            for i in lo..=hi {
                let t = (self.pos - i as f64).abs() * self.scale;
                let k = t as usize;
                if k >= last {
                    continue;
                }
                let f = (t - k as f64) as f32;
                let c = self.table[k] + (self.table[k + 1] - self.table[k]) * f;
                acc += self.buf[i] * c;
            }
            out.push(acc);
            self.pos += self.step;
        }
        // 只留下一个输出还要用的历史。
        let keep_from = (self.pos.floor() as isize - self.half as isize).max(0) as usize;
        if keep_from > 0 {
            self.buf.drain(..keep_from);
            self.pos -= keep_from as f64;
        }
    }

    /// 清掉历史（打断播放时用）。
    pub fn reset(&mut self) {
        self.buf.clear();
        self.buf.resize(self.half, 0.0);
        self.pos = self.half as f64;
    }
}

pub fn i16_to_f32(v: &[i16]) -> Vec<f32> {
    v.iter().map(|&s| s as f32 / 32768.0).collect()
}

pub fn f32_to_i16(v: &[f32]) -> Vec<i16> {
    v.iter().map(|&s| (s * 32768.0).round().clamp(-32768.0, 32767.0) as i16).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f64, secs: f64) -> Vec<f32> {
        (0..(rate as f64 * secs) as usize).map(|i| (2.0 * PI * hz * i as f64 / rate as f64).sin() as f32 * 0.5).collect()
    }

    fn amp(v: &[f32]) -> f32 {
        let mid = &v[v.len() / 4..v.len() * 3 / 4];
        (mid.iter().map(|x| x * x).sum::<f32>() / mid.len() as f32).sqrt() * 2f32.sqrt()
    }

    fn run(from: u32, to: u32, hz: f64) -> Vec<f32> {
        let x = sine(from, hz, 1.0);
        let mut r = Resampler::new(from, to);
        let mut y = Vec::new();
        for c in x.chunks(441) {
            r.process(c, &mut y);
        }
        y
    }

    #[test]
    fn keeps_length_and_passband() {
        for (from, to) in [(48000, 16000), (44100, 16000), (24000, 48000), (24000, 44100)] {
            let y = run(from, to, 1000.0);
            let expect = to as f64;
            assert!((y.len() as f64 - expect).abs() < expect * 0.01, "{from}->{to}: {}", y.len());
            assert!((amp(&y) - 0.5).abs() < 0.01, "{from}->{to}: amp {}", amp(&y));
        }
    }

    #[test]
    fn removes_what_the_target_cannot_hold() {
        let y = run(48000, 16000, 12000.0); // 高于 8 kHz 奈奎斯特
        assert!(amp(&y) < 0.005, "alias amp {}", amp(&y));
    }
}
