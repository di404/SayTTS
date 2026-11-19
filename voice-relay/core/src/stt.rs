//! 转写：Fish Audio transcribe-stream-1，经实时 WebSocket（wss://api.fish.audio/v1/asr/realtime）。
//!
//! 协议：每条消息是一个二进制帧，里面一个 MessagePack map，`type` 字段区分种类。
//! 断句由本地断句器负责，所以用 manual 模式：一句说完发 `finalize`，服务端回一条 `committed` 定稿。
//!
//! 计费按连接打开的时长算（$0.36/小时），没说话时连接开着也计费；客户端 30 秒不发消息服务端就关连接。
//! 所以只在说话前后保持连接：开始时连一次确认 key 和网络可用，之后空闲 `IDLE_CLOSE` 就主动断开，
//! 下次开口再连。还没拿到定稿的句子保留音频，连上（或断了重连）后整句重发。

use std::{collections::VecDeque, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use reqwest_websocket::{CloseCode, Message, Upgrade};
use rmpv::Value;
use serde_json::json;
use tokio::sync::mpsc;

use crate::net::{Net, WS_URL};

pub const STT_MODEL: &str = "transcribe-stream-1";
pub const STT_RATE: u32 = 16000;
const SEND_SAMPLES: usize = 640; // 每 40 ms 发一次（文档建议 20–40 ms）
const DROP_AFTER: Duration = Duration::from_secs(5); // 说完 5 秒还没定稿，就不再等这句
const IDLE_CLOSE: Duration = Duration::from_secs(10); // 没话说这么久就断开，不再计费
/// 服务端的专有名词表限制：最多 100 条，每条最多 32 个字符。
const MAX_TERMS: usize = 100;
const MAX_TERM_CHARS: usize = 32;

/// 转写器报告给程序的事件。
#[derive(Debug, Clone, PartialEq)]
pub enum SttEvent {
    Connected,
    /// 连接失败或断了，正在重连。参数是原因。
    Disconnected(String),
    Interim(String),
    Final(String),
    /// 一句话没能送到转写服务（一直连不上），已放弃。参数是这句的时长（毫秒）。
    Lost(u64),
}

/// 断句器交给转写器的输入。
#[derive(Debug, Clone, PartialEq)]
pub enum Gate {
    /// 20 ms、16 kHz 单声道。只在说话时送来。
    Frame(Vec<i16>),
    /// 一句说完。
    End,
}

#[derive(Clone)]
pub struct Transcriber {
    tx: mpsc::UnboundedSender<Msg>,
}

impl Transcriber {
    /// 启动转写器。事件从 `events` 发出；程序丢掉所有 `Transcriber` 克隆时它就停止并关闭连接。
    /// `vocabulary` 是专有名词表，每条连接建立时发给服务。
    pub fn start(net: Net, vocabulary: Vec<String>, events: mpsc::UnboundedSender<SttEvent>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let terms: Vec<String> = vocabulary
            .iter()
            .map(|t| t.trim().chars().take(MAX_TERM_CHARS).collect::<String>())
            .filter(|t| !t.is_empty())
            .take(MAX_TERMS)
            .collect();
        let actor = Actor {
            net,
            terms: terms.into(),
            me: tx.downgrade(),
            out: events,
            cur: None,
            connecting: false,
            down: true,
            sentences: VecDeque::new(),
            speaking: false,
            pending: Vec::new(),
            ids: 0,
            retry: 0,
            idle_gen: 0,
        };
        tokio::spawn(actor.run(rx));
        Self { tx }
    }

    pub fn send(&self, g: Gate) {
        let _ = self.tx.send(match g {
            Gate::Frame(f) => Msg::Frame(f),
            Gate::End => Msg::End,
        });
    }
}

enum Msg {
    Frame(Vec<i16>),
    End,
    Connected(Result<Session>),
    Session(u64, FromSession),
    Drop(u64),
    Retry,
    Idle(u64),
}

enum FromSession {
    Interim(String),
    Final(String),
    Closed(String),
}

enum Out {
    Audio(Vec<i16>),
    Finalize,
}

struct Session {
    id: u64,
    tx: mpsc::UnboundedSender<Out>,
}

impl Session {
    fn audio(&self, pcm: &[i16]) {
        for c in pcm.chunks(SEND_SAMPLES) {
            let _ = self.tx.send(Out::Audio(c.to_vec()));
        }
    }
    fn finalize(&self) {
        let _ = self.tx.send(Out::Finalize);
    }
}

struct Sentence {
    id: u64,
    audio: Vec<i16>,
    ended: bool,
    /// 已经在当前连接上发过 finalize。连接断了就清掉，换新连接时整句重发。
    sent: bool,
}

struct Actor {
    net: Net,
    terms: Arc<[String]>,
    me: mpsc::WeakUnboundedSender<Msg>,
    out: mpsc::UnboundedSender<SttEvent>,
    cur: Option<Session>,
    connecting: bool,
    /// 上次报告的是“断开”（或还没连上过）。连上时据此报告 Connected。
    down: bool,
    sentences: VecDeque<Sentence>, // 还没拿到定稿的句子
    speaking: bool,
    pending: Vec<i16>, // 攒够 40 ms 再发
    ids: u64,
    /// 连续失败次数：连接失败、或连上后还没出过结果就被关。出过结果才清零。
    retry: u32,
    /// 每有动静加一；空闲计时到点时编号没变，才断开。
    idle_gen: u64,
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        self.connect();
        while let Some(m) = rx.recv().await {
            match m {
                Msg::Frame(f) => self.frame(f),
                Msg::End => self.end_of_speech(),
                Msg::Connected(r) => self.connected(r),
                Msg::Session(id, ev) => self.session_event(id, ev),
                Msg::Drop(id) => self.drop_sentence(id),
                Msg::Retry => {
                    if self.cur.is_none() && (self.down || self.busy()) {
                        self.connect();
                    }
                }
                Msg::Idle(g) => {
                    if g == self.idle_gen && !self.busy() {
                        self.cur = None; // 写任务发 Close 后退出
                    }
                }
            }
        }
        // 所有 Transcriber 都丢了：Session 随之 drop，写任务发 Close 后退出。
    }

    /// 正在说话，或有句子在等定稿。
    fn busy(&self) -> bool {
        self.speaking || !self.sentences.is_empty()
    }

    fn arm_idle(&mut self) {
        self.idle_gen += 1;
        if self.busy() || self.cur.is_none() {
            return;
        }
        let (g, me) = (self.idle_gen, self.me.clone());
        tokio::spawn(async move {
            tokio::time::sleep(IDLE_CLOSE).await;
            if let Some(me) = me.upgrade() {
                let _ = me.send(Msg::Idle(g));
            }
        });
    }

    fn frame(&mut self, f: Vec<i16>) {
        if !self.speaking {
            self.speaking = true;
            self.idle_gen += 1;
            self.ids += 1;
            self.sentences.push_back(Sentence { id: self.ids, audio: Vec::new(), ended: false, sent: false });
            if self.cur.is_none() {
                self.connect(); // 连上后整句重发
            }
        }
        if let Some(s) = self.sentences.back_mut() {
            s.audio.extend_from_slice(&f);
        }
        self.pending.extend_from_slice(&f);
        if self.pending.len() >= SEND_SAMPLES {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if let Some(cur) = &self.cur
            && !self.pending.is_empty()
        {
            cur.audio(&self.pending);
        }
        self.pending.clear();
    }

    fn end_of_speech(&mut self) {
        if !self.speaking {
            return;
        }
        self.speaking = false;
        self.flush();
        if let Some(s) = self.sentences.back_mut() {
            s.ended = true;
            let id = s.id;
            if let Some(cur) = &self.cur {
                cur.finalize();
                s.sent = true;
            }
            let me = self.me.clone();
            tokio::spawn(async move {
                tokio::time::sleep(DROP_AFTER).await;
                if let Some(me) = me.upgrade() {
                    let _ = me.send(Msg::Drop(id));
                }
            });
        }
    }

    fn connect(&mut self) {
        if self.connecting {
            return;
        }
        self.connecting = true;
        self.ids += 1;
        let (id, net, me, terms) = (self.ids, self.net.clone(), self.me.clone(), self.terms.clone());
        tokio::spawn(async move {
            let r = open_session(&net, &terms, id, me.clone()).await;
            if let Some(me) = me.upgrade() {
                let _ = me.send(Msg::Connected(r));
            }
        });
    }

    fn connected(&mut self, r: Result<Session>) {
        self.connecting = false;
        match r {
            Ok(s) => self.attach(s),
            Err(e) => {
                self.down = true;
                let _ = self.out.send(SttEvent::Disconnected(format!("{e:#}")));
                self.schedule_retry();
            }
        }
    }

    /// 按连续失败次数退避后再连：第一次马上，之后 1, 2, 4, 8, 16 秒。
    /// 服务端可能接受连接后立刻关掉（余额不足、key 被停用），不退避就会不停地重连。
    fn schedule_retry(&mut self) {
        let wait = match self.retry {
            0 => Duration::ZERO,
            n => Duration::from_secs(1 << (n - 1).min(4)),
        };
        self.retry += 1;
        let me = self.me.clone();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            if let Some(me) = me.upgrade() {
                let _ = me.send(Msg::Retry);
            }
        });
    }

    /// 一句说完太久还没定稿：不再等它。如果它从没送到当前连接上（一直连不上），这句话就丢了，告诉界面。
    fn drop_sentence(&mut self, id: u64) {
        let Some(i) = self.sentences.iter().position(|s| s.id == id) else { return };
        let s = self.sentences.remove(i).expect("下标来自 position");
        if !s.sent {
            let ms = s.audio.len() as u64 * 1000 / STT_RATE as u64;
            let _ = self.out.send(SttEvent::Lost(ms));
        }
        self.arm_idle();
    }

    /// 用一条新连接接替当前连接，并重发还没定稿的句子。
    fn attach(&mut self, s: Session) {
        for sent in &mut self.sentences {
            s.audio(&sent.audio);
            if sent.ended {
                s.finalize();
                sent.sent = true;
            }
        }
        self.pending.clear(); // 当前句已经整句重发过
        self.cur = Some(s);
        if self.down {
            self.down = false;
            let _ = self.out.send(SttEvent::Connected);
        }
        self.arm_idle();
    }

    fn session_event(&mut self, id: u64, ev: FromSession) {
        if !self.cur.as_ref().is_some_and(|s| s.id == id) {
            return; // 已经换掉或主动断开的连接
        }
        match ev {
            FromSession::Final(text) => {
                self.retry = 0;
                if let Some(i) = self.sentences.iter().position(|s| s.ended) {
                    self.sentences.remove(i);
                }
                let text = text.trim().to_string();
                if !text.is_empty() {
                    let _ = self.out.send(SttEvent::Final(text));
                }
                self.arm_idle();
            }
            FromSession::Interim(text) => {
                self.retry = 0;
                let _ = self.out.send(SttEvent::Interim(text));
            }
            FromSession::Closed(reason) => {
                self.cur = None;
                self.sentences.iter_mut().for_each(|s| s.sent = false);
                // 没有话在等时被关（例如到了单次会话的时长上限），等下次开口再连
                if self.busy() {
                    self.down = true;
                    let _ = self.out.send(SttEvent::Disconnected(reason));
                    self.schedule_retry();
                }
            }
        }
    }
}

/// 打开一条转写连接：握手、发 config、等 session_started，然后起读写两个任务。
async fn open_session(net: &Net, terms: &[String], id: u64, me: mpsc::WeakUnboundedSender<Msg>) -> Result<Session> {
    let resp = net
        .ws
        .get(WS_URL)
        .header("Authorization", net.auth())
        .header("model", STT_MODEL)
        .upgrade()
        .send()
        .await
        .map_err(|e| anyhow!("转写连接失败：{}", net.describe(&e)))?;
    let mut ws = resp
        .into_websocket()
        .await
        .map_err(|e| anyhow!("转写握手失败：{}", net.describe(&e)))?;
    let mut config = json!({
        "type": "config",
        "audio": { "format": "pcm_s16le", "sample_rate": STT_RATE, "channels": 1 },
        "turn_detection": { "mode": "manual" },
        // 说话人标记和 [笑声] 之类的标签会被读出来，关掉
        "results": { "drafts": true, "diarize": false, "tag_audio_events": false },
    });
    if !terms.is_empty() {
        config["context"] = json!({ "terms": terms });
    }
    ws.send(Message::Binary(encode(&rmpv::ext::to_value(&config)?).into())).await.context("发送 config 失败")?;
    let started = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match ws.next().await {
                Some(Ok(m)) => {
                    let Some(v) = parse(m) else { continue };
                    match kind(&v) {
                        "session_started" => return Ok(()),
                        "error" => bail!("转写服务拒绝：{}", net.redact(str_of(&v, "message"))),
                        _ => {}
                    }
                }
                Some(Err(e)) => bail!("转写 config 失败：{}", net.describe(&e)),
                None => bail!("转写连接在 config 时关闭"),
            }
        }
    });
    started.await.context("等 session_started 超时")??;

    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    tokio::spawn(async move {
        while let Some(o) = rx.recv().await {
            let msg = match o {
                Out::Audio(pcm) => {
                    let bytes: Vec<u8> = pcm.iter().flat_map(|v| v.to_le_bytes()).collect();
                    Value::Map(vec![("type".into(), "audio".into()), ("data".into(), Value::Binary(bytes))])
                }
                Out::Finalize => Value::Map(vec![("type".into(), "finalize".into())]),
            };
            if sink.send(Message::Binary(encode(&msg).into())).await.is_err() {
                return;
            }
        }
        // Session 被丢弃：正常关闭这条连接，停止计费。
        let _ = sink.send(Message::Close { code: CloseCode::Normal, reason: String::new() }).await;
        let _ = sink.close().await;
    });
    let redact = net.clone();
    tokio::spawn(async move {
        let send = |ev: FromSession| match me.upgrade() {
            Some(me) => me.send(Msg::Session(id, ev)).is_ok(),
            None => false,
        };
        let mut last_error = String::new();
        let reason = loop {
            match stream.next().await {
                Some(Ok(Message::Close { code, reason })) => {
                    break format!("{} {} {}", u16::from(code), reason, last_error).trim().to_string();
                }
                Some(Ok(m)) => {
                    let Some(v) = parse(m) else { continue };
                    let alive = match kind(&v) {
                        "committed" => send(FromSession::Final(str_of(&v, "text").to_string())),
                        "draft" => send(FromSession::Interim(str_of(&v, "text").to_string())),
                        "error" => {
                            last_error = redact.redact(str_of(&v, "message"));
                            true
                        }
                        _ => true,
                    };
                    if !alive {
                        return;
                    }
                }
                Some(Err(e)) => break e.to_string(),
                None => break "连接关闭".to_string(),
            }
        };
        send(FromSession::Closed(reason));
    });
    Ok(Session { id, tx })
}

fn encode(v: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, v).expect("写入内存不会失败");
    buf
}

/// 服务端只发二进制帧，每帧一个 MessagePack map。
fn parse(m: Message) -> Option<Value> {
    match m {
        Message::Binary(b) => rmpv::decode::read_value(&mut &b[..]).ok(),
        _ => None,
    }
}

fn field<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_map()?.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v)
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    field(v, key).and_then(Value::as_str).unwrap_or("")
}

fn kind(v: &Value) -> &str {
    str_of(v, "type")
}
