//! 声卡：列设备、麦克风采集、播放。
//! cpal 的 Stream 在有的平台不能跨线程，所以每个流在自己的线程里创建、运行、销毁。
//! 播放用设备自己的采样率，24 kHz 的 TTS 音频在送进去之前重采样。

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc as std_mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use serde::Serialize;

use crate::resample::Resampler;

#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub default: bool,
}

pub fn inputs() -> Result<Vec<DeviceInfo>> {
    list(true)
}

pub fn outputs() -> Result<Vec<DeviceInfo>> {
    list(false)
}

fn list(input: bool) -> Result<Vec<DeviceInfo>> {
    let host = cpal::default_host();
    let default = if input { host.default_input_device() } else { host.default_output_device() }
        .and_then(|d| d.id().ok())
        .map(|i| i.to_string());
    let devices: Vec<cpal::Device> =
        if input { host.input_devices()?.collect() } else { host.output_devices()?.collect() };
    let mut out = Vec::new();
    for d in devices {
        let (Ok(id), Ok(desc)) = (d.id(), d.description()) else { continue };
        let id = id.to_string();
        out.push(DeviceInfo { default: default.as_deref() == Some(id.as_str()), id, name: desc.name().to_string() });
    }
    Ok(out)
}

/// `id` 为空时用系统默认设备。
fn find(input: bool, id: &str) -> Result<cpal::Device> {
    let host = cpal::default_host();
    if id.is_empty() {
        return if input { host.default_input_device() } else { host.default_output_device() }
            .ok_or_else(|| anyhow!(if input { "没有默认麦克风" } else { "没有默认输出设备" }));
    }
    let parsed = id.parse().map_err(|_| anyhow!("设备 id 无效"))?;
    host.device_by_id(&parsed).ok_or_else(|| anyhow!("找不到设备（可能已拔出）：{id}"))
}

pub fn device_name(input: bool, id: &str) -> String {
    find(input, id).ok().and_then(|d| d.description().ok()).map(|d| d.name().to_string()).unwrap_or_default()
}

/// 在独立线程里持有一个 cpal 流；drop 时停止。
struct StreamThread {
    stop: Option<std_mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl StreamThread {
    fn spawn(build: impl FnOnce() -> Result<cpal::Stream> + Send + 'static) -> Result<Self> {
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();
        let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
        let thread = std::thread::Builder::new().name("audio-stream".into()).spawn(move || {
            let stream = match build().and_then(|s| s.play().map(|_| s).context("启动音频流失败")) {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            let _ = stop_rx.recv(); // 等停止信号或发送端被丢弃
            drop(stream);
        })?;
        ready_rx.recv().unwrap_or_else(|_| Err(anyhow!("音频线程意外退出")))?;
        Ok(Self { stop: Some(stop_tx), thread: Some(thread) })
    }
}

impl Drop for StreamThread {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// 音频流运行中报的错。
#[derive(Debug, Clone)]
pub struct StreamError {
    pub message: String,
    /// 设备没了（拔出、被系统移除）：这条流不会再工作，要重新打开设备。
    pub lost: bool,
}

/// 把 cpal 的错误分类。只是通知性质、流还在正常工作的（跟随默认设备切换、偶发欠载、实时优先级被拒）返回 None。
fn classify(what: &str, e: cpal::Error) -> Option<StreamError> {
    use cpal::ErrorKind as K;
    match e.kind() {
        K::DeviceChanged | K::Xrun | K::RealtimeDenied => None,
        K::DeviceNotAvailable | K::StreamInvalidated => Some(StreamError { message: format!("{what}断开了：{e}"), lost: true }),
        _ => Some(StreamError { message: format!("{what}出错：{e}"), lost: false }),
    }
}

/// 系统默认设备的 id，没有时为空字符串。用来发现默认设备变了。
pub fn default_id(input: bool) -> String {
    let host = cpal::default_host();
    if input { host.default_input_device() } else { host.default_output_device() }
        .and_then(|d| d.id().ok())
        .map(|i| i.to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- 采集

pub struct Capture {
    pub rate: u32,
    pub name: String,
    _stream: StreamThread,
}

/// 打开麦克风。`on_audio` 在音频线程里收到单声道 f32 和设备采样率，要尽快返回。
pub fn capture(id: &str, on_audio: impl FnMut(&[f32]) + Send + 'static, on_error: impl Fn(StreamError) + Send + 'static) -> Result<Capture> {
    let device = find(true, id)?;
    let name = device.description().map(|d| d.name().to_string()).unwrap_or_default();
    let cfg = device.default_input_config().context("读不到麦克风的格式")?;
    let rate = cfg.sample_rate();
    let format = cfg.sample_format();
    let config: StreamConfig = cfg.config();
    let stream = StreamThread::spawn(move || {
        let err = move |e: cpal::Error| {
            if let Some(e) = classify("麦克风", e) {
                on_error(e);
            }
        };
        Ok(match format {
            SampleFormat::F32 => build_in::<f32>(&device, config, on_audio, err)?,
            SampleFormat::I16 => build_in::<i16>(&device, config, on_audio, err)?,
            SampleFormat::I32 => build_in::<i32>(&device, config, on_audio, err)?,
            SampleFormat::U16 => build_in::<u16>(&device, config, on_audio, err)?,
            SampleFormat::F64 => build_in::<f64>(&device, config, on_audio, err)?,
            f => bail!("不支持的麦克风格式 {f:?}"),
        })
    })?;
    Ok(Capture { rate, name, _stream: stream })
}

fn build_in<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut on_audio: impl FnMut(&[f32]) + Send + 'static,
    err: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let ch = config.channels as usize;
    let mut mono = Vec::new();
    let stream = device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _| {
                mono.clear();
                mono.extend(data.chunks(ch).map(|f| f.iter().map(|s| f32::from_sample(*s)).sum::<f32>() / ch as f32));
                on_audio(&mono);
            },
            err,
            None,
        )
        .context("打开麦克风失败（是否已授权麦克风权限？）")?;
    Ok(stream)
}

// ---------------------------------------------------------------- 播放

/// 一路播放。音频按设备采样率放进环形队列，声卡回调从里面取；没有数据时输出静音。
pub struct Output {
    pub rate: u32,
    /// 送进来的音频是多少 Hz（TTS 后端的输出采样率）
    src_rate: u32,
    pub name: String,
    ring: Arc<Ring>,
    rs: Resampler,
    written: u64,
    _sink: Sink,
}

struct Ring {
    buf: Mutex<VecDeque<f32>>,
    played: AtomicU64, // 已经播出的真实采样数（不算补的静音）
    volume: AtomicU32, // f32 的位，播放时乘上
}

impl Ring {
    fn new() -> Arc<Self> {
        Arc::new(Self { buf: Mutex::new(VecDeque::new()), played: AtomicU64::new(0), volume: AtomicU32::new(1f32.to_bits()) })
    }
    fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }
}

/// 改一路输出的音量，立即生效。0.0–1.0。
#[derive(Clone)]
pub struct Volume(Arc<Ring>);

impl Volume {
    pub fn set(&self, v: f32) {
        self.0.volume.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

/// 一路输出此刻是不是在出声：队列里还有没播的音频，且音量不为 0。
#[derive(Clone)]
pub struct Activity(Arc<Ring>);

impl Activity {
    pub fn audible(&self) -> bool {
        self.0.volume() > 0.0 && !self.0.buf.lock().unwrap().is_empty()
    }
}

enum Sink {
    Device(#[allow(dead_code)] StreamThread),
    Wav(WavSink),
}

impl Output {
    /// 打开一个输出设备。`id` 为空时用系统默认输出。`src_rate` 是之后 `push` 进来的音频的采样率。
    pub fn open(id: &str, src_rate: u32, on_error: impl Fn(StreamError) + Send + 'static) -> Result<Self> {
        let device = find(false, id)?;
        let name = device.description().map(|d| d.name().to_string()).unwrap_or_default();
        let cfg = device.default_output_config().context("读不到输出设备的格式")?;
        let rate = cfg.sample_rate();
        let format = cfg.sample_format();
        let config: StreamConfig = cfg.config();
        let ring = Ring::new();
        let r = ring.clone();
        let stream = StreamThread::spawn(move || {
            let err = move |e: cpal::Error| {
                if let Some(e) = classify("输出设备", e) {
                    on_error(e);
                }
            };
            Ok(match format {
                SampleFormat::F32 => build_out::<f32>(&device, config, r, err)?,
                SampleFormat::I16 => build_out::<i16>(&device, config, r, err)?,
                SampleFormat::I32 => build_out::<i32>(&device, config, r, err)?,
                SampleFormat::U16 => build_out::<u16>(&device, config, r, err)?,
                SampleFormat::F64 => build_out::<f64>(&device, config, r, err)?,
                f => bail!("不支持的输出格式 {f:?}"),
            })
        })?;
        Ok(Self { rate, src_rate, name, ring, rs: Resampler::new(src_rate, rate), written: 0, _sink: Sink::Device(stream) })
    }

    /// 不出声的输出：按实时节奏消费音频并写进 WAV（测试用）。
    pub fn wav(path: PathBuf, rate: u32, src_rate: u32) -> Result<Self> {
        let ring = Ring::new();
        let sink = WavSink::start(path.clone(), rate, ring.clone())?;
        Ok(Self {
            rate,
            src_rate,
            name: path.display().to_string(),
            ring,
            rs: Resampler::new(src_rate, rate),
            written: 0,
            _sink: Sink::Wav(sink),
        })
    }

    /// 送入 `src_rate` Hz 的 TTS 音频。
    pub fn push(&mut self, pcm: &[i16]) {
        // TTS 峰值接近满幅，插值后采样之间的峰会超过 1.0 而削波；留 1 dB 余量。
        const HEADROOM: f32 = 0.89;
        let input: Vec<f32> = pcm.iter().map(|&s| s as f32 / 32768.0 * HEADROOM).collect();
        let mut out = Vec::with_capacity(input.len() * self.rate as usize / self.src_rate as usize + 8);
        self.rs.process(&input, &mut out);
        self.written += out.len() as u64;
        self.ring.buf.lock().unwrap().extend(out);
    }

    pub fn volume(&self) -> Volume {
        Volume(self.ring.clone())
    }

    pub fn activity(&self) -> Activity {
        Activity(self.ring.clone())
    }

    /// 已送入的采样总数（按设备采样率）。下一个送入的采样会在这个位置播出。
    pub fn written(&self) -> u64 {
        self.written
    }

    /// 已经播出的采样总数。
    pub fn played(&self) -> u64 {
        self.ring.played.load(Ordering::Acquire)
    }

    /// 马上停止：丢掉还没播的音频。
    pub fn cut(&mut self) {
        let mut q = self.ring.buf.lock().unwrap();
        let dropped = q.len() as u64;
        q.clear();
        // 丢掉的部分算作“已播”，这样 played 和 written 对得上
        self.ring.played.fetch_add(dropped, Ordering::AcqRel);
        drop(q);
        self.rs.reset();
    }

    pub fn finish_wav(self) {
        if let Sink::Wav(w) = self._sink {
            w.finish();
        }
    }
}

fn build_out<T>(
    device: &cpal::Device,
    config: StreamConfig,
    ring: Arc<Ring>,
    err: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = config.channels as usize;
    let stream = device
        .build_output_stream::<T, _, _>(
            config,
            move |data: &mut [T], _| {
                let vol = ring.volume();
                let mut q = ring.buf.lock().unwrap();
                let frames = data.len() / ch;
                let n = frames.min(q.len());
                for (i, frame) in data.chunks_mut(ch).enumerate() {
                    let v = if i < n { q.pop_front().unwrap_or(0.0) } else { 0.0 };
                    let s = T::from_sample(v * vol);
                    frame.fill(s);
                }
                drop(q);
                ring.played.fetch_add(n as u64, Ordering::AcqRel);
            },
            err,
            None,
        )
        .context("打开输出设备失败")?;
    Ok(stream)
}

struct WavSink {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WavSink {
    fn start(path: PathBuf, rate: u32, ring: Arc<Ring>) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let spec = hound::WavSpec { channels: 1, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(&path, spec)?;
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::spawn(move || {
            let tick = Duration::from_millis(10);
            let per_tick = (rate / 100) as usize;
            let start = std::time::Instant::now();
            let mut n = 0u64;
            while !s.load(Ordering::Acquire) {
                n += 1;
                let due = start + tick * n as u32;
                if let Some(d) = due.checked_duration_since(std::time::Instant::now()) {
                    std::thread::sleep(d);
                }
                let vol = ring.volume();
                let mut q = ring.buf.lock().unwrap();
                let k = per_tick.min(q.len());
                let real: Vec<f32> = q.drain(..k).collect();
                drop(q);
                ring.played.fetch_add(k as u64, Ordering::AcqRel);
                for i in 0..per_tick {
                    let v = real.get(i).copied().unwrap_or(0.0) * vol;
                    let _ = w.write_sample((v * 32767.0).round().clamp(-32768.0, 32767.0) as i16);
                }
            }
            let _ = w.finalize();
        });
        Ok(Self { stop, thread: Some(thread) })
    }

    fn finish(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for WavSink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
