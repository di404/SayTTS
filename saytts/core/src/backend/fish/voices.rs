//! 预置的中文声音：Fish Audio 公开声音库里使用量高、不模仿真人的几个（2026-11-20 从 GET /model?language=zh 挑选）。
//! `id` 是 TTS 请求里的 reference_id。

use anyhow::{Result, anyhow, bail};
use serde_json::Value;

use super::{API, Fish};
use crate::{backend::PresetVoice, tts::Voice};

const fn v(id: &'static str, name: &'static str, desc: &'static str) -> PresetVoice {
    PresetVoice { id, name, desc }
}

/// 第一个是默认声音。
pub const VOICES: [PresetVoice; 11] = [
    v("5c353fdb312f4888836a9a5680099ef0", "女大学生", "明亮"),
    v("faccba1a8ac54016bcfc02761285e67f", "温柔女声", "温柔"),
    v("fbe02f8306fc4d3d915e9871722a39d5", "嘉岚", "柔和、聊天感"),
    v("bf6c479f5a384b8d857310030035824b", "活力女声", "活泼"),
    v("6ce7ea8ada884bf3889fa7c7fb206691", "茉莉", "轻柔"),
    v("e46d11a7ce9147de9eca8f4c778d1883", "播报女声", "清晰"),
    v("29a7eea9ed484ef6b175da9bcfb49979", "自然男声", "平稳"),
    v("665e031efe27435780ebfa56cc7e0e0d", "月半猫", "温和"),
    v("83bc284d2a3848c8a37f6d9a9ab11cd8", "科普男声", "清晰"),
    v("2926cb350f1a426d800bf8c360c3cb94", "新闻男声", "播报感"),
    v("b4bdf5dc66004241a21ff2df165bf442", "解说男声", "有表现力"),
];

/// 读账号里已训练好、能用来朗读的声音。
pub async fn account_voices(fish: &Fish) -> Result<Vec<Voice>> {
    let resp = fish
        .http
        .http
        .get(format!("{API}/model?self=true&page_size=100"))
        .header("Authorization", fish.auth())
        .send()
        .await
        .map_err(|e| anyhow!("读取我的声音失败：{}", fish.describe(&e)))?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| anyhow!("读取我的声音失败：{}", fish.describe(&e)))?;
    if !status.is_success() {
        bail!("读取我的声音失败：HTTP {}：{}", status.as_u16(), fish.redact(body["message"].as_str().unwrap_or("")));
    }
    let items = body["items"].as_array().cloned().unwrap_or_default();
    Ok(items
        .iter()
        .filter(|m| m["type"] == "tts" && m["state"] == "trained")
        .filter_map(|m| {
            Some(Voice { id: m["_id"].as_str()?.to_string(), name: m["title"].as_str()?.to_string(), desc: String::new() })
        })
        .collect())
}
