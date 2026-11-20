//! Fish Audio TTS：POST /v1/tts，分块返回 24 kHz 单声道 PCM16（小端）。

use anyhow::anyhow;
use futures_util::{StreamExt, future::BoxFuture};
use serde_json::{Value, json};

use super::{API, Fish, voices};
use crate::tts::{OnAudio, SpeechError, Tts, TtsRequest, Voice};

const RATE: u32 = 24000;
/// 第一个是默认模型。s2.1-pro-free 不收费，但不保证延迟；s2.1-pro 同样的音质、有延迟保证；
/// drama-3 表现力更强，价格是 s2.1-pro 的 3 倍多。
pub const MODELS: [&str; 3] = ["s2.1-pro-free", "s2.1-pro", "drama-3"];

impl Tts for Fish {
    fn rate(&self) -> u32 {
        RATE
    }

    fn request(&self, req: TtsRequest, fresh: bool, on_audio: OnAudio) -> BoxFuture<'static, Result<(), SpeechError>> {
        let fish = self.clone();
        Box::pin(async move { request(&fish, req, fresh, on_audio).await })
    }

    /// 发一个不计费的轻请求，让连接池里有一条建好的连接。
    fn warm(&self) -> BoxFuture<'static, ()> {
        let fish = self.clone();
        Box::pin(async move {
            let url = format!("{API}/model?page_size=1&self=true");
            if let Ok(r) = fish.http.http.get(url).header("Authorization", fish.auth()).send().await {
                let _ = r.bytes().await;
            }
        })
    }

    fn account_voices(&self) -> BoxFuture<'static, anyhow::Result<Vec<Voice>>> {
        let fish = self.clone();
        Box::pin(async move { voices::account_voices(&fish).await })
    }
}

/// 发一个合成请求。只有收到过音频才返回 Ok。
async fn request(fish: &Fish, req: TtsRequest, fresh: bool, mut on_audio: OnAudio) -> Result<(), SpeechError> {
    let body = json!({
        "text": req.text,
        "reference_id": req.voice,
        "format": "pcm",
        "sample_rate": RATE,
        // 实测 balanced 首包比 low 还快（约 0.65 s 对 1.0 s）
        "latency": "balanced",
        "normalize": true,
        "language": "zh-CN",
    });
    let client = if fresh { &fish.http.fresh } else { &fish.http.http };
    let resp = client
        .post(format!("{API}/v1/tts"))
        .header("Authorization", fish.auth())
        // 模型名写错时服务端会默默改用 s2.1-pro，所以只发 MODELS 里的名字
        .header("model", &req.model)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("TTS 请求失败：{}", fish.describe(&e)))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let e = anyhow!("TTS HTTP {}：{}", status.as_u16(), short(&fish.redact(&api_error(&text))));
        // 4xx（key 无效、余额不足、参数错）重发也没用；5xx 和 429（并发数满了）值得重发
        return Err(if status.is_client_error() && status.as_u16() != 429 {
            SpeechError::Fatal(e)
        } else {
            SpeechError::Retryable(e)
        });
    }

    let mut odd: Option<u8> = None; // 上一块剩下的半个采样
    let mut got_audio = false;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow!("TTS 连接中断：{}", fish.describe(&e)))?;
        let mut bytes = chunk.to_vec();
        if let Some(b) = odd.take() {
            bytes.insert(0, b);
        }
        if bytes.len() % 2 == 1 {
            odd = bytes.pop();
        }
        let pcm: Vec<i16> = bytes.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        if !pcm.is_empty() {
            got_audio = true;
            on_audio(pcm);
        }
    }
    if !got_audio {
        return Err(anyhow!("TTS 没有返回音频").into());
    }
    Ok(())
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
