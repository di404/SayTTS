//! WAV 读写（测试和探针用）。

use std::path::Path;

use anyhow::{Result, bail};

use crate::resample::{Resampler, f32_to_i16};

/// 读成单声道 i16，并重采样到 `rate`。
pub fn read_mono(path: &Path, rate: u32) -> Result<Vec<i16>> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    let ch = spec.channels as usize;
    let samples: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, 16) => r.samples::<i16>().map(|s| s.map(|v| v as f32 / 32768.0)).collect::<Result<_, _>>()?,
        (hound::SampleFormat::Float, 32) => r.samples::<f32>().collect::<Result<_, _>>()?,
        (f, b) => bail!("不支持的 WAV 格式：{f:?} {b} bit"),
    };
    let mono: Vec<f32> = samples.chunks(ch).map(|c| c.iter().sum::<f32>() / ch as f32).collect();
    let mut out = Vec::new();
    let mut rs = Resampler::new(spec.sample_rate, rate);
    rs.process(&mono, &mut out);
    rs.process(&vec![0.0; spec.sample_rate as usize / 10], &mut out); // 推出滤波器尾部
    out.truncate((mono.len() as u64 * rate as u64 / spec.sample_rate as u64) as usize);
    Ok(f32_to_i16(&out))
}

pub fn write_mono(path: &Path, pcm: &[i16], rate: u32) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let spec = hound::WavSpec { channels: 1, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec)?;
    for &s in pcm {
        w.write_sample(s)?;
    }
    w.finalize()?;
    Ok(())
}
