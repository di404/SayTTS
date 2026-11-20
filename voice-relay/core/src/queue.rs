//! 朗读队列：每句定稿一到就开始合成，不等前一句播完；按到达顺序首尾相接地播放。
//! 字幕在这句第一个有声采样播出时显示，播完时隐藏。

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc};

use crate::{
    audio::Output,
    tts::{self, Tts, TtsRequest},
};

const LEAD_THRESHOLD: u16 = 300; // 开头静音：|采样| 不超过这个值
const LEAD_KEEP: usize = 240; // 保留第一个有声采样前 10 ms
/// 一句先攒这么多毫秒的有声音频（或整句合成完）才开始播。经代理实测，平均合成速度是实时的 4 倍以上，
/// 但首块之后有时停 0.2–0.5 秒才来下一块；从首块就开始播，句子中间会断。先攒够再播，停顿落在开口之前。
const START_BUFFER_MS: u32 = 300;
const TTS_TIMEOUT: Duration = Duration::from_secs(30);
const WARM_AFTER: Duration = Duration::from_secs(20); // 空闲这么久后，开口时先预热连接

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VoiceConfig {
    pub model: String,
    pub voice: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueueEvent {
    Added { id: u64, text: String },
    Playing { id: u64, text: String },
    /// `stall_ms`：开始播放到播完用的时间，比这句音频长出多少。合成跟不上播放、中途断音时大于 0。
    Done { id: u64, stall_ms: u64 },
    Failed { id: u64, error: String },
    Cleared,
}

#[derive(Clone)]
pub struct SpeechQueue {
    tx: mpsc::UnboundedSender<Cmd>,
}

enum Cmd {
    Say(String),
    Stop,
    Voice(VoiceConfig),
    Warm,
}

impl SpeechQueue {
    /// `live` 和 `monitor` 要按 `tts.rate()` 打开。
    pub fn start(
        tts: Arc<dyn Tts>,
        voice: VoiceConfig,
        live: Output,
        monitor: Option<Output>,
        events: mpsc::UnboundedSender<QueueEvent>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = Actor {
            start_buffer: start_buffer(tts.as_ref()),
            tts,
            voice,
            live,
            monitor,
            events,
            items: VecDeque::new(),
            wake: Arc::new(Notify::new()),
            ids: 0,
            last_request: None,
        };
        tokio::spawn(actor.run(rx));
        Self { tx }
    }

    pub fn say(&self, text: String) {
        let _ = self.tx.send(Cmd::Say(text));
    }

    /// 停止朗读并清空队列。
    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn set_voice(&self, v: VoiceConfig) {
        let _ = self.tx.send(Cmd::Voice(v));
    }

    /// 刚开口时调用：如果连接可能已经冷了，先发一个轻请求把它建好。
    pub fn warm(&self) {
        let _ = self.tx.send(Cmd::Warm);
    }
}

enum State {
    Running,
    Done,
    Failed(String),
}

struct Audio {
    chunks: Mutex<(Vec<Vec<i16>>, State)>,
}

struct Item {
    id: u64,
    text: String,
    audio: Arc<Audio>,
    task: tokio::task::JoinHandle<()>,
    fed: usize,
    lead_done: bool,
    start: Option<u64>, // 第一个采样在直播输出上的位置
    end: Option<u64>,   // 最后一个采样之后的位置；有值表示整句已送完
    shown: Option<Instant>, // 开始播放的时刻
}

struct Actor {
    tts: Arc<dyn Tts>,
    /// 一句攒够这么多个采样才开始播
    start_buffer: usize,
    voice: VoiceConfig,
    live: Output,
    monitor: Option<Output>,
    events: mpsc::UnboundedSender<QueueEvent>,
    items: VecDeque<Item>,
    wake: Arc<Notify>,
    ids: u64,
    last_request: Option<Instant>,
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        loop {
            let busy = !self.items.is_empty();
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(c) => self.command(c),
                    None => break,
                },
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(10)), if busy => {}
            }
            self.feed();
            self.advance();
        }
        self.stop();
    }

    fn command(&mut self, c: Cmd) {
        match c {
            Cmd::Say(text) => self.say(text),
            Cmd::Stop => self.stop(),
            Cmd::Voice(v) => self.voice = v,
            Cmd::Warm => {
                if self.last_request.is_none_or(|t| t.elapsed() > WARM_AFTER) {
                    self.last_request = Some(Instant::now());
                    tokio::spawn(self.tts.warm());
                }
            }
        }
    }

    fn say(&mut self, text: String) {
        self.ids += 1;
        let id = self.ids;
        self.last_request = Some(Instant::now());
        let audio = Arc::new(Audio { chunks: Mutex::new((Vec::new(), State::Running)) });
        let (a, wake, engine) = (audio.clone(), self.wake.clone(), self.tts.clone());
        let req = TtsRequest { model: self.voice.model.clone(), voice: self.voice.voice.clone(), text: text.clone() };
        let task = tokio::spawn(async move {
            let r = tokio::time::timeout(
                TTS_TIMEOUT,
                tts::synthesize(&engine, &req, |c| {
                    a.chunks.lock().unwrap().0.push(c);
                    wake.notify_one();
                }),
            )
            .await;
            a.chunks.lock().unwrap().1 = match r {
                Ok(Ok(())) => State::Done,
                Ok(Err(e)) => State::Failed(format!("{e:#}")),
                Err(_) => State::Failed("合成超时".into()),
            };
            wake.notify_one();
        });
        let _ = self.events.send(QueueEvent::Added { id, text: text.clone() });
        self.items.push_back(Item { id, text, audio, task, fed: 0, lead_done: false, start: None, end: None, shown: None });
    }

    /// 按顺序把已合成的音频送进输出；前一句没送完，不送后一句。
    fn feed(&mut self) {
        for i in 0..self.items.len() {
            if self.items[i].end.is_some() {
                continue;
            }
            let (new, state) = {
                let it = &self.items[i];
                let g = it.audio.chunks.lock().unwrap();
                let state = match &g.1 {
                    State::Running => None,
                    State::Done => Some(None),
                    State::Failed(e) => Some(Some(e.clone())),
                };
                if it.start.is_none() && state.is_none() && voiced_len(&g.0) < self.start_buffer {
                    break; // 这句还没攒够
                }
                (g.0[it.fed..].to_vec(), state)
            };
            self.items[i].fed += new.len();
            for c in new {
                self.push(i, &c);
            }
            match state {
                None => break,
                Some(err) => {
                    let pos = self.live.written();
                    let it = &mut self.items[i];
                    it.end = Some(pos);
                    it.start.get_or_insert(pos);
                    if let Some(e) = err {
                        let _ = self.events.send(QueueEvent::Failed { id: it.id, error: e });
                    }
                }
            }
        }
    }

    fn push(&mut self, i: usize, chunk: &[i16]) {
        let it = &mut self.items[i];
        let chunk = if it.lead_done {
            chunk
        } else {
            match chunk.iter().position(|v| v.unsigned_abs() > LEAD_THRESHOLD) {
                Some(k) => {
                    it.lead_done = true;
                    &chunk[k.saturating_sub(LEAD_KEEP)..]
                }
                None => return, // 整块都是开头静音
            }
        };
        it.start.get_or_insert(self.live.written());
        self.live.push(chunk);
        if let Some(m) = &mut self.monitor {
            m.push(chunk);
        }
    }

    /// 按直播输出的播放进度显示、隐藏字幕，并移走播完的句子。
    fn advance(&mut self) {
        let played = self.live.played();
        let rate = self.live.rate as f64;
        while let Some(head) = self.items.front_mut() {
            let Some(start) = head.start else { break };
            let has_audio = head.end.is_none_or(|e| e > start);
            if head.shown.is_none() && has_audio && played >= start {
                head.shown = Some(Instant::now());
                let _ = self.events.send(QueueEvent::Playing { id: head.id, text: head.text.clone() });
            }
            match head.end {
                Some(e) if played >= e => {
                    // 断音时声卡补静音，played 不走，所以实际用时会比音频长
                    let audio = Duration::from_secs_f64((e - start) as f64 / rate);
                    let stall_ms = head.shown.map_or(0, |t| t.elapsed().saturating_sub(audio).as_millis() as u64);
                    let id = head.id;
                    self.items.pop_front();
                    let _ = self.events.send(QueueEvent::Done { id, stall_ms });
                }
                _ => break,
            }
        }
    }

    fn stop(&mut self) {
        if self.items.is_empty() {
            return;
        }
        for it in self.items.drain(..) {
            it.task.abort();
        }
        self.live.cut();
        if let Some(m) = &mut self.monitor {
            m.cut();
        }
        let _ = self.events.send(QueueEvent::Cleared);
    }
}

fn start_buffer(tts: &dyn Tts) -> usize {
    (tts.rate() * START_BUFFER_MS / 1000) as usize
}

/// 去掉开头静音后（规则和 `push` 一样）有多少个采样。
fn voiced_len(chunks: &[Vec<i16>]) -> usize {
    for (j, c) in chunks.iter().enumerate() {
        if let Some(k) = c.iter().position(|v| v.unsigned_abs() > LEAD_THRESHOLD) {
            return c.len() - k.saturating_sub(LEAD_KEEP) + chunks[j + 1..].iter().map(Vec::len).sum::<usize>();
        }
    }
    0
}

/// 试听：用给定的声音读一句，放到 `device`（空字符串是系统默认输出），播完才返回。
pub async fn preview(tts: &Arc<dyn Tts>, v: &VoiceConfig, device: &str, volume: f32, text: &str) -> anyhow::Result<()> {
    let mut out = Output::open(device, tts.rate(), |_| {})?;
    let need = start_buffer(tts.as_ref());
    let req = TtsRequest { model: v.model.clone(), voice: v.voice.clone(), text: text.to_string() };
    out.volume().set(volume);
    let mut held: Vec<Vec<i16>> = Vec::new(); // 和朗读队列一样，攒够再开始播
    let mut started = false;
    let synth = tts::synthesize(tts, &req, |c| {
        if started {
            out.push(&c);
            return;
        }
        held.push(c);
        if voiced_len(&held) >= need {
            started = true;
            held.drain(..).for_each(|h| out.push(&h));
        }
    });
    tokio::time::timeout(TTS_TIMEOUT, synth).await.map_err(|_| anyhow::anyhow!("合成超时"))??;
    held.drain(..).for_each(|h| out.push(&h));
    // 等它播完；设备停了也不会一直等
    let left = (out.written() - out.played()) as f64 / out.rate as f64;
    let deadline = Instant::now() + Duration::from_secs_f64(left) + Duration::from_secs(2);
    while out.played() < out.written() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}
