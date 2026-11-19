//! Fish Audio TTS：POST /v1/tts，分块返回 24 kHz 单声道 PCM16（小端）。

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::net::{API, Net};

pub const TTS_RATE: u32 = 24000;
/// 第一个是默认模型。s2.1-pro-free 不收费，但不保证延迟；s2.1-pro 同样的音质、有延迟保证；
/// drama-3 表现力更强，价格是 s2.1-pro 的 3 倍多。
pub const MODELS: [&str; 3] = ["s2.1-pro-free", "s2.1-pro", "drama-3"];

/// 网络偶尔会让一个请求迟迟没有音频。一个请求发出这么久还没有音频，就在新连接上补发一个，
/// 哪个先出声用哪个，其余的立即取消。
const HEDGE_AFTER: Duration = Duration::from_secs(2);
/// 一句最多发这么多个请求（第一个、补发的、失败后重发的）。
const MAX_ATTEMPTS: usize = 3;

/// 合成一句话。每收到一块音频调用一次 `on_audio`（24 kHz 单声道 i16），音频只来自一个请求。
/// 返回时整句已合成完。调用方 drop 这个 future 就会中止所有请求。
pub async fn synthesize(
    net: &Net,
    model: &str,
    voice: &str,
    text: &str,
    mut on_audio: impl FnMut(Vec<i16>),
) -> Result<()> {
    enum Msg {
        Audio(usize, Vec<i16>),
        End(usize, Result<()>),
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
    let start = |i: usize, client: &reqwest::Client| {
        let (tx, net, client) = (tx.clone(), net.clone(), client.clone());
        let (model, voice, text) = (model.to_string(), voice.to_string(), text.to_string());
        tokio::spawn(async move {
            let audio = tx.clone();
            let r = request(&net, &client, &model, &voice, &text, |c| {
                let _ = audio.send(Msg::Audio(i, c));
            })
            .await;
            let _ = tx.send(Msg::End(i, r));
        })
    };
    let mut attempts = Attempts(Vec::new());
    let mut running = 0;
    let mut winner = None;
    let mut launch = Some(String::new()); // 要发新请求时，是发它的原因
    // 4xx（key 无效、余额不足、参数错）：不再发新请求，已经发出的等它们结束
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
            attempts.0.push(start(i, if i == 0 { &net.http } else { &net.fresh }));
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
                        return r;
                    }
                    if winner.is_some() {
                        continue;
                    }
                    running -= 1;
                    let e = r.err().unwrap_or_else(|| anyhow!("TTS 没有返回音频"));
                    if !retryable(&e) {
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

/// HTTP 4xx（key 无效、余额不足、参数错）重发也没用；网络错误、5xx 和 429（并发数满了）值得重发。
fn retryable(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HttpError>().is_none_or(|h| h.status >= 500 || h.status == 429)
}

#[derive(Debug)]
struct HttpError {
    status: u16,
    message: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TTS HTTP {}：{}", self.status, self.message)
    }
}

impl std::error::Error for HttpError {}

/// 发一个合成请求。只有收到过音频才返回 Ok。
async fn request(
    net: &Net,
    client: &reqwest::Client,
    model: &str,
    voice: &str,
    text: &str,
    mut on_audio: impl FnMut(Vec<i16>),
) -> Result<()> {
    let body = json!({
        "text": text,
        "reference_id": voice,
        "format": "pcm",
        "sample_rate": TTS_RATE,
        // 实测 balanced 首包比 low 还快（约 0.65 s 对 1.0 s）
        "latency": "balanced",
        "normalize": true,
        "language": "zh-CN",
    });
    let resp = client
        .post(format!("{API}/v1/tts"))
        .header("Authorization", net.auth())
        // 模型名写错时服务端会默默改用 s2.1-pro，所以只发 MODELS 里的名字
        .header("model", model)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("TTS 请求失败：{}", net.describe(&e)))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(HttpError { status: status.as_u16(), message: short(&net.redact(&api_error(&text))) }.into());
    }

    let mut odd: Option<u8> = None; // 上一块剩下的半个采样
    let mut got_audio = false;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow!("TTS 连接中断：{}", net.describe(&e)))?;
        let mut bytes = chunk.to_vec();
        if let Some(b) = odd.take() {
            bytes.insert(0, b);
        }
        if bytes.len() % 2 == 1 {
            odd = bytes.pop();
        }
        let pcm: Vec<i16> = bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        if !pcm.is_empty() {
            got_audio = true;
            on_audio(pcm);
        }
    }
    if !got_audio {
        bail!("TTS 没有返回音频");
    }
    Ok(())
}

/// 预热：发一个不计费的轻请求，让连接池里有一条建好的连接。
pub async fn warm(net: &Net) {
    let url = format!("{API}/model?page_size=1&self=true");
    if let Ok(r) = net.http.get(url).header("Authorization", net.auth()).send().await {
        let _ = r.bytes().await;
    }
}

/// Fish 的错误体是 `{"status": .., "message": ".."}`。
fn api_error(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.to_string())
}

fn short(s: &str) -> String {
    s.chars().take(300).collect()
}
