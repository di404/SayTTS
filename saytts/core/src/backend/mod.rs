//! 朗读和转写平台的注册表。接一家新平台：在这里加一个子目录，实现 `Tts` / `Stt`，把它的 `PROVIDER` 加进 `PROVIDERS`。

pub mod fish;

use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;

use crate::{net::HttpKit, queue::VoiceConfig, stt::Stt, tts::Tts};

/// 建一个后端需要的东西。
pub struct Creds {
    pub key: String,
    pub http: HttpKit,
}

/// 一个平台。界面和设置用 `id` 认它；不需要 key 就能列出的信息（模型、预置声音）放在这里。
pub struct Provider {
    /// 存进设置的名字，例如 "fish"
    pub id: &'static str,
    /// 界面上显示的名字
    pub name: &'static str,
    /// 没保存 key 时读这个环境变量
    pub key_env: &'static str,
    /// 平台不提供朗读就是 None
    pub tts: Option<TtsSpec>,
    /// 平台不提供转写就是 None
    pub stt: Option<SttSpec>,
}

pub struct TtsSpec {
    /// 第一个是默认模型
    pub models: &'static [&'static str],
    /// 第一个是默认声音
    pub voices: &'static [PresetVoice],
    pub build: fn(&Creds) -> Result<Arc<dyn Tts>>,
}

pub struct SttSpec {
    pub build: fn(&Creds) -> Result<Arc<dyn Stt>>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct PresetVoice {
    pub id: &'static str,
    pub name: &'static str,
    pub desc: &'static str,
}

/// 第一个是默认平台。
pub const PROVIDERS: &[Provider] = &[fish::PROVIDER];

pub fn find(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// 默认的朗读平台和转写平台。
pub fn default_tts() -> &'static Provider {
    PROVIDERS.iter().find(|p| p.tts.is_some()).expect("至少有一个朗读平台")
}

pub fn default_stt() -> &'static Provider {
    PROVIDERS.iter().find(|p| p.stt.is_some()).expect("至少有一个转写平台")
}

impl Provider {
    /// 这个平台的默认模型和声音。不提供朗读的平台返回 None。
    pub fn default_voice(&self) -> Option<VoiceConfig> {
        let t = self.tts.as_ref()?;
        Some(VoiceConfig { model: t.models.first()?.to_string(), voice: t.voices.first()?.id.to_string() })
    }

    pub fn info(&self) -> ProviderInfo {
        ProviderInfo { id: self.id, name: self.name, key_env: self.key_env, tts: self.tts.is_some(), stt: self.stt.is_some() }
    }
}

/// 发给界面的平台信息。
#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub key_env: &'static str,
    pub tts: bool,
    pub stt: bool,
}
