//! Fish Audio：朗读用 POST /v1/tts，转写用实时 WebSocket。两者用同一个 API key。

mod stt;
mod tts;
mod voices;

use std::sync::Arc;

use anyhow::{Result, ensure};

use super::{Creds, Provider, SttSpec, TtsSpec};
use crate::net::{HttpKit, describe};

const API: &str = "https://api.fish.audio";
const WS_URL: &str = "wss://api.fish.audio/v1/asr/realtime";

pub const PROVIDER: Provider = Provider {
    id: "fish",
    name: "Fish Audio",
    key_env: "FISH_API_KEY",
    tts: Some(TtsSpec { models: &tts::MODELS, voices: &voices::VOICES, build: |c| Ok(Arc::new(Fish::new(c)?)) }),
    stt: Some(SttSpec { build: |c| Ok(Arc::new(Fish::new(c)?)) }),
};

/// 一个 Fish Audio 账号的连接。朗读和转写都实现在它上面。
#[derive(Clone)]
struct Fish {
    http: HttpKit,
    key: Arc<str>,
}

impl Fish {
    fn new(c: &Creds) -> Result<Self> {
        let key = c.key.trim();
        ensure!(!key.is_empty(), "没有 Fish Audio API key");
        Ok(Self { http: c.http.clone(), key: key.into() })
    }

    /// `Authorization` 头的值。
    fn auth(&self) -> String {
        format!("Bearer {}", self.key)
    }

    /// 把文本里出现的 key 换成 ***，用于错误信息和日志。
    fn redact(&self, s: &str) -> String {
        s.replace(&*self.key, "***")
    }

    /// 错误连同它的所有原因，去掉 key。
    fn describe(&self, e: &(dyn std::error::Error + 'static)) -> String {
        self.redact(&describe(e))
    }
}
