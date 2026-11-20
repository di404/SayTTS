//! 朗读后端的接口，和不分平台的合成策略（补发、重试）。
//! 各平台只实现“发一个请求、把 PCM 交出来”，见 `backend::*::tts`。

use std::{sync::Arc, time::Duration};

use anyhow::{Result, anyhow, bail};
use futures_util::future::BoxFuture;
use serde::Serialize;
use tokio::{sync::mpsc, task::JoinHandle};

/// 网络偶尔会让一个请求迟迟没有音频。一个请求发出这么久还没有音频，就在新连接上补发一个，
/// 哪个先出声用哪个，其余的立即取消。
const HEDGE_AFTER: Duration = Duration::from_secs(2);
/// 一句最多发这么多个请求（第一个、补发的、失败后重发的）。
const MAX_ATTEMPTS: usize = 3;

pub type OnAudio = Box<dyn FnMut(Vec<i16>) + Send>;

/// 一个朗读平台。返回的 future 自带所需的数据（'static），可以直接交给 tokio::spawn。
pub trait Tts: Send + Sync {
    /// 交出来的 PCM 是多少 Hz，单声道 i16。
    fn rate(&self) -> u32;

    /// 发一个合成请求，每收到一块音频调用一次 `on_audio`。只有收到过音频才返回 Ok。
    /// `fresh` 为真时用新连接（补发用：旧连接可能卡住了）。
    fn request(&self, req: TtsRequest, fresh: bool, on_audio: OnAudio) -> BoxFuture<'static, Result<(), SpeechError>>;

    /// 预热：让连接池里有一条建好的连接。默认什么也不做。
    fn warm(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }

    /// 账号里自己的声音（自己训练或上传的）。平台没有这个概念就返回空。
    fn account_voices(&self) -> BoxFuture<'static, Result<Vec<Voice>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

#[derive(Debug, Clone)]
pub struct TtsRequest {
    pub model: String,
    pub voice: String,
    pub text: String,
}

/// 界面上列出的一个声音。
#[derive(Debug, Clone, Serialize)]
pub struct Voice {
    pub id: String,
    pub name: String,
    pub desc: String,
}

/// 后端报告的错误，分两类：重发也没用的（key 无效、余额不足、参数错），和值得重发的（网络、服务端忙）。
#[derive(Debug)]
pub enum SpeechError {
    Fatal(anyhow::Error),
    Retryable(anyhow::Error),
}

impl SpeechError {
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Retryable(_))
    }
    pub fn into_inner(self) -> anyhow::Error {
        match self {
            Self::Fatal(e) | Self::Retryable(e) => e,
        }
    }
}

/// 没有特别说明的错误（网络断了之类）按值得重发处理。
impl From<anyhow::Error> for SpeechError {
    fn from(e: anyhow::Error) -> Self {
        Self::Retryable(e)
    }
}

/// 合成一句话。每收到一块音频调用一次 `on_audio`（`tts.rate()` Hz 单声道 i16），音频只来自一个请求。
/// 返回时整句已合成完。调用方 drop 这个 future 就会中止所有请求。
pub async fn synthesize(tts: &Arc<dyn Tts>, req: &TtsRequest, mut on_audio: impl FnMut(Vec<i16>)) -> Result<()> {
    enum Msg {
        Audio(usize, Vec<i16>),
        End(usize, Result<(), SpeechError>),
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
    let start = |i: usize| {
        let audio = tx.clone();
        let end = tx.clone();
        let fut = tts.request(
            req.clone(),
            i > 0,
            Box::new(move |c| {
                let _ = audio.send(Msg::Audio(i, c));
            }),
        );
        tokio::spawn(async move {
            let r = fut.await;
            let _ = end.send(Msg::End(i, r));
        })
    };
    let mut attempts = Attempts(Vec::new());
    let mut running = 0;
    let mut winner = None;
    let mut launch = Some(String::new()); // 要发新请求时，是发它的原因
    // 致命错误：不再发新请求，已经发出的等它们结束
    let mut fatal: Option<anyhow::Error> = None;
    let mut last: Option<anyhow::Error> = None;
    let hedge = tokio::time::sleep(HEDGE_AFTER);
    tokio::pin!(hedge);
    loop {
        if let Some(why) = launch.take() {
            let i = attempts.0.len();
            if cfg!(debug_assertions) && i > 0 {
                eprintln!("[tts] 第 {} 个请求：{why}", i + 1);
            }
            attempts.0.push(start(i));
            running += 1;
            // 每发一个请求都重新计时：失败后重发的那个也要有人兜底
            hedge.as_mut().reset(tokio::time::Instant::now() + HEDGE_AFTER);
        }
        let more = winner.is_none() && fatal.is_none() && attempts.0.len() < MAX_ATTEMPTS;
        tokio::select! {
            m = rx.recv() => match m {
                Some(Msg::Audio(i, c)) => {
                    if winner.is_none() {
                        winner = Some(i);
                        attempts.abort_except(i);
                    }
                    if winner == Some(i) {
                        on_audio(c);
                    }
                }
                Some(Msg::End(i, r)) => {
                    if winner == Some(i) {
                        return r.map_err(SpeechError::into_inner);
                    }
                    if winner.is_some() {
                        continue;
                    }
                    running -= 1;
                    let e = r.err().unwrap_or_else(|| SpeechError::Retryable(anyhow!("TTS 没有返回音频")));
                    let retry = e.retryable();
                    let e = e.into_inner();
                    if !retry {
                        fatal.get_or_insert(e);
                    } else if more {
                        launch = Some(format!("上一个失败（{e:#}）"));
                        continue;
                    } else {
                        last = Some(e);
                    }
                    if running == 0 {
                        return Err(fatal.or(last).unwrap_or_else(|| anyhow!("TTS 没有返回音频")));
                    }
                }
                None => bail!("TTS 请求意外结束"),
            },
            _ = &mut hedge, if more && running == 1 => launch = Some(format!("{} 秒没有音频", HEDGE_AFTER.as_secs())),
        }
    }
}

/// 一句话的所有请求任务。drop 时全部中止。
struct Attempts(Vec<JoinHandle<()>>);

impl Attempts {
    fn abort_except(&self, keep: usize) {
        for (i, t) in self.0.iter().enumerate() {
            if i != keep {
                t.abort();
            }
        }
    }
}

impl Drop for Attempts {
    fn drop(&mut self) {
        for t in &self.0 {
            t.abort();
        }
    }
}
