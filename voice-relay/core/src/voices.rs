//! 预置的中文声音：Fish Audio 公开声音库里使用量高、不模仿真人的几个（2026-11-20 从 GET /model?language=zh 挑选）。
//! `id` 是 TTS 请求里的 reference_id。

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use serde_json::Value;

use crate::net::{API, Net};

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Voice {
    pub id: &'static str,
    pub name: &'static str,
    pub trait_zh: &'static str,
}

/// 第一个是默认声音。
pub const VOICES: [Voice; 11] = [
    Voice { id: "5c353fdb312f4888836a9a5680099ef0", name: "女大学生", trait_zh: "明亮" },
    Voice { id: "faccba1a8ac54016bcfc02761285e67f", name: "温柔女声", trait_zh: "温柔" },
    Voice { id: "fbe02f8306fc4d3d915e9871722a39d5", name: "嘉岚", trait_zh: "柔和、聊天感" },
    Voice { id: "bf6c479f5a384b8d857310030035824b", name: "活力女声", trait_zh: "活泼" },
    Voice { id: "6ce7ea8ada884bf3889fa7c7fb206691", name: "茉莉", trait_zh: "轻柔" },
    Voice { id: "e46d11a7ce9147de9eca8f4c778d1883", name: "播报女声", trait_zh: "清晰" },
    Voice { id: "29a7eea9ed484ef6b175da9bcfb49979", name: "自然男声", trait_zh: "平稳" },
    Voice { id: "665e031efe27435780ebfa56cc7e0e0d", name: "月半猫", trait_zh: "温和" },
    Voice { id: "83bc284d2a3848c8a37f6d9a9ab11cd8", name: "科普男声", trait_zh: "清晰" },
    Voice { id: "2926cb350f1a426d800bf8c360c3cb94", name: "新闻男声", trait_zh: "播报感" },
    Voice { id: "b4bdf5dc66004241a21ff2df165bf442", name: "解说男声", trait_zh: "有表现力" },
];

/// 账号里自己的声音（自己训练或上传的）。
#[derive(Debug, Clone, Serialize)]
pub struct OwnVoice {
    pub id: String,
    pub name: String,
}

/// 读账号里已训练好、能用来朗读的声音。
pub async fn own_voices(net: &Net) -> Result<Vec<OwnVoice>> {
    let resp = net
        .http
        .get(format!("{API}/model?self=true&page_size=100"))
        .header("Authorization", net.auth())
        .send()
        .await
        .map_err(|e| anyhow!("读取我的声音失败：{}", net.describe(&e)))?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| anyhow!("读取我的声音失败：{}", net.describe(&e)))?;
    if !status.is_success() {
        bail!("读取我的声音失败：HTTP {}：{}", status.as_u16(), body["message"].as_str().unwrap_or(""));
    }
    let items = body["items"].as_array().cloned().unwrap_or_default();
    Ok(items
        .iter()
        .filter(|m| m["type"] == "tts" && m["state"] == "trained")
        .filter_map(|m| Some(OwnVoice { id: m["_id"].as_str()?.to_string(), name: m["title"].as_str()?.to_string() }))
        .collect())
}
