//! 整条链路：麦克风 → 断句 → 转写 → 朗读队列 → 输出设备。

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::Result;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::{
    audio::{self, Capture, Output, Volume},
    endpointer::{Endpointer, FRAME, Mode},
    net::Net,
    queue::{QueueEvent, SpeechQueue, VoiceConfig},
    resample::Resampler,
    stt::{Gate, STT_RATE, SttEvent, Transcriber},
    wav,
};

/// 监听的队列播空后，声卡缓冲、外放和房间余响还要这么久才真正安静下来。
const ECHO_TAIL: Duration = Duration::from_millis(300);

pub enum Input {
    Mic(String),
    /// 测试用：按实时节奏播放一个 WAV 当作麦克风，放完后一直送静音。
    Wav(PathBuf),
}

pub enum Sink {
    Device(String),
    /// 测试用：不出声，写进 WAV。
    Wav(PathBuf),
}

pub struct Config {
    pub input: Input,
    pub output: Sink,
    pub monitor: Option<Sink>,
    pub mode: Mode,
    pub gate_ms: u32,
    /// 自动断句时一句最长多少毫秒，0 表示不限。
    pub max_ms: u32,
    pub voice: VoiceConfig,
    /// 专有名词表，发给转写服务。
    pub vocabulary: Vec<String>,
    /// 监听音量，0.0–1.0。
    pub monitor_volume: f32,
    /// 监听出声时不收音（半双工）：防止外放的朗读被麦克风收进去再读一遍，代价是这段时间说的话会丢。
    pub echo_gate: bool,
}

/// 发给界面的事件。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Started { input: String, output: String, monitor: Option<String> },
    SttConnected,
    SttDisconnected { reason: String },
    SpeechStart,
    SpeechEnd,
    Interim { text: String },
    Final { text: String },
    Added { id: u64, text: String },
    Playing { id: u64, text: String },
    Done { id: u64, stall_ms: u64 },
    Failed { id: u64, error: String },
    Cleared,
    Level { level: f32, threshold: f32, speaking: bool },
    /// 一句话因为一直连不上转写服务而丢掉。`ms` 是这句的时长。
    Lost { ms: u64 },
    /// 麦克风或输出设备断开，这次运行已经不能工作，需要重新打开设备。
    DeviceLost { message: String },
    Error { message: String },
}

pub struct Pipeline {
    queue: SpeechQueue,
    monitor_volume: Option<Volume>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    wav_feeder: Option<JoinHandle<()>>,
    _capture: Option<Capture>,
}

impl Pipeline {
    /// 需要在 tokio 运行时里调用。`held` 是按键说话的按键状态。
    pub fn start(net: Net, cfg: Config, held: Arc<AtomicBool>, events: mpsc::UnboundedSender<Event>) -> Result<Self> {
        let err_tx = events.clone();
        let on_error = move |e: audio::StreamError| {
            let _ = err_tx.send(if e.lost { Event::DeviceLost { message: e.message } } else { Event::Error { message: e.message } });
        };
        let open = |s: &Sink| -> Result<Output> {
            match s {
                Sink::Device(id) => Output::open(id, on_error.clone()),
                Sink::Wav(p) => Output::wav(p.clone(), 48000),
            }
        };
        let live = open(&cfg.output)?;
        // 监听只是给主播自己听的：打不开（耳机没插）不影响直播，先不监听
        // 提示在 Started 之后发：界面收到 Started 之前不认这次运行的事件
        let mut warning = None;
        let monitor = cfg.monitor.as_ref().and_then(|m| match open(m) {
            Ok(o) => Some(o),
            Err(e) => {
                warning = Some(format!("监听设备打不开，先不监听：{e:#}"));
                None
            }
        });
        let (out_name, mon_name) = (live.name.clone(), monitor.as_ref().map(|m| m.name.clone()));
        let monitor_volume = monitor.as_ref().map(Output::volume);
        let echo = monitor.as_ref().filter(|_| cfg.echo_gate).map(Output::activity);
        if let Some(v) = &monitor_volume {
            v.set(cfg.monitor_volume);
        }

        // 先打开所有设备，再连转写服务（按连接时长计费）：设备打不开时不白连一次
        // 输入：声卡回调或 WAV 线程把音频交给处理线程
        let stop = Arc::new(AtomicBool::new(false));
        let (atx, arx) = std_mpsc::channel::<Vec<f32>>();
        let (capture, wav_feeder, rate, in_name) = match &cfg.input {
            Input::Mic(id) => {
                let tx = atx.clone();
                let c = audio::capture(id, move |buf| { let _ = tx.send(buf.to_vec()); }, on_error.clone())?;
                let (rate, name) = (c.rate, c.name.clone());
                (Some(c), None, rate, name)
            }
            Input::Wav(path) => {
                let pcm = wav::read_mono(path, STT_RATE)?;
                let (tx, stop) = (atx.clone(), stop.clone());
                let h = std::thread::spawn(move || {
                    let step = (STT_RATE / 100) as usize; // 10 ms
                    let start = Instant::now();
                    let mut n = 0u32;
                    let mut pos = 0usize;
                    while !stop.load(Ordering::Acquire) {
                        n += 1;
                        if let Some(d) = (start + Duration::from_millis(10) * n).checked_duration_since(Instant::now()) {
                            std::thread::sleep(d);
                        }
                        let buf: Vec<f32> = (0..step)
                            .map(|i| pcm.get(pos + i).map_or(0.0, |&s| s as f32 / 32768.0))
                            .collect();
                        pos += step;
                        if tx.send(buf).is_err() {
                            break;
                        }
                    }
                });
                (None, Some(h), STT_RATE, path.display().to_string())
            }
        };
        drop(atx);

        // 朗读队列
        let (qtx, mut qrx) = mpsc::unbounded_channel();
        let queue = SpeechQueue::start(net.clone(), cfg.voice.clone(), live, monitor, qtx);
        let ev = events.clone();
        tokio::spawn(async move {
            while let Some(e) = qrx.recv().await {
                let _ = ev.send(match e {
                    QueueEvent::Added { id, text } => Event::Added { id, text },
                    QueueEvent::Playing { id, text } => Event::Playing { id, text },
                    QueueEvent::Done { id, stall_ms } => Event::Done { id, stall_ms },
                    QueueEvent::Failed { id, error } => Event::Failed { id, error },
                    QueueEvent::Cleared => Event::Cleared,
                });
            }
        });

        // 转写：定稿直接进朗读队列
        let (stx, mut srx) = mpsc::unbounded_channel();
        let transcriber = Transcriber::start(net, cfg.vocabulary.clone(), stx);
        let (ev, q) = (events.clone(), queue.clone());
        tokio::spawn(async move {
            while let Some(e) = srx.recv().await {
                let _ = ev.send(match e {
                    SttEvent::Connected => Event::SttConnected,
                    SttEvent::Disconnected(reason) => Event::SttDisconnected { reason },
                    SttEvent::Interim(text) => Event::Interim { text },
                    SttEvent::Lost(ms) => Event::Lost { ms },
                    SttEvent::Final(text) => {
                        q.say(text.clone());
                        Event::Final { text }
                    }
                });
            }
        });


        let mut ep = Endpointer::new(cfg.mode, cfg.gate_ms, cfg.max_ms);
        let (stop2, ev, q) = (stop.clone(), events.clone(), queue.clone());
        let worker = std::thread::Builder::new().name("audio-process".into()).spawn(move || {
            let mut rs = Resampler::new(rate, STT_RATE);
            let mut pending: Vec<f32> = Vec::new();
            let mut frames = 0u64;
            let mut last_echo: Option<Instant> = None;
            let gate = |g: Gate, ev: &mpsc::UnboundedSender<Event>| {
                match &g {
                    Gate::End => {
                        let _ = ev.send(Event::SpeechEnd);
                    }
                    Gate::Frame(_) => {}
                }
                transcriber.send(g);
            };
            loop {
                if stop2.load(Ordering::Acquire) {
                    ep.flush(&mut |g| gate(g, &ev));
                    break;
                }
                let buf = match arx.recv_timeout(Duration::from_millis(100)) {
                    Ok(b) => b,
                    Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
                };
                rs.process(&buf, &mut pending);
                // 监听外放时麦克风会收进朗读的声音，这段时间的输入不送去转写
                if echo.as_ref().is_some_and(|a| a.audible()) {
                    last_echo = Some(Instant::now());
                }
                let muted = last_echo.is_some_and(|t| t.elapsed() < ECHO_TAIL);
                let mut off = 0;
                while pending.len() - off >= FRAME {
                    let frame: Vec<i16> = pending[off..off + FRAME]
                        .iter()
                        .map(|&s| (s * 32768.0).round().clamp(-32768.0, 32767.0) as i16)
                        .collect();
                    off += FRAME;
                    let was = ep.speaking();
                    let level = ep.push(&frame, held.load(Ordering::Acquire), muted, &mut |g| gate(g, &ev));
                    if !was && ep.speaking() {
                        let _ = ev.send(Event::SpeechStart);
                        q.warm();
                    }
                    frames += 1;
                    if frames.is_multiple_of(3) {
                        let _ = ev.send(Event::Level { level, threshold: ep.threshold(), speaking: ep.speaking() });
                    }
                }
                pending.drain(..off);
            }
        })?;

        let _ = events.send(Event::Started { input: in_name, output: out_name, monitor: mon_name });
        if let Some(message) = warning {
            let _ = events.send(Event::Error { message });
        }
        Ok(Self { queue, monitor_volume, stop, worker: Some(worker), wav_feeder, _capture: capture })
    }

    /// 停止正在读的句子并清空队列。
    pub fn stop_speech(&self) {
        self.queue.stop();
    }

    /// 打字输入，或重试读失败的句子：和语音定稿进同一个队列。
    pub fn say(&self, text: String) {
        self.queue.say(text);
    }

    pub fn set_monitor_volume(&self, v: f32) {
        if let Some(m) = &self.monitor_volume {
            m.set(v);
        }
    }

    pub fn set_voice(&self, v: VoiceConfig) {
        self.queue.set_voice(v);
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.wav_feeder.take() {
            let _ = h.join();
        }
        self._capture.take();
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        self.queue.stop();
    }
}
