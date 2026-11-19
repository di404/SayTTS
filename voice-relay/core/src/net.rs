//! 连接 Fish Audio API 的客户端：TTS 用普通 HTTP（可走 HTTP/2），转写用 WebSocket（必须 HTTP/1.1）。
//! 两个客户端用同一个代理设置。代理为空时用系统代理（macOS 系统设置 / Windows 注册表 / 环境变量）。

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};

pub const API: &str = "https://api.fish.audio";
pub const WS_URL: &str = "wss://api.fish.audio/v1/asr/realtime";

#[derive(Clone)]
pub struct Net {
    pub http: reqwest::Client,
    /// 不留空闲连接：每个请求新建一条连接。用于 TTS 的补发请求。
    pub fresh: reqwest::Client,
    pub ws: reqwest::Client,
    key: Arc<str>,
}

impl Net {
    /// `proxy` 形如 `http://127.0.0.1:7890` 或 `socks5h://127.0.0.1:7890`；空字符串表示用系统代理。
    pub fn new(key: &str, proxy: &str) -> Result<Self> {
        let key = key.trim();
        anyhow::ensure!(!key.is_empty(), "没有 Fish Audio API key");
        let proxy = proxy.trim();
        let build = |http1: bool, pool: bool| -> Result<reqwest::Client> {
            let mut b = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .tcp_nodelay(true)
                .pool_idle_timeout(Duration::from_secs(300));
            if http1 {
                b = b.http1_only();
            }
            if !pool {
                b = b.pool_max_idle_per_host(0);
            }
            if !proxy.is_empty() {
                b = b.proxy(reqwest::Proxy::all(proxy).context("代理地址无效")?);
            }
            Ok(b.build()?)
        };
        Ok(Self { http: build(false, true)?, fresh: build(false, false)?, ws: build(true, true)?, key: key.into() })
    }

    /// `Authorization` 头的值。
    pub(crate) fn auth(&self) -> String {
        format!("Bearer {}", self.key)
    }
}

impl Net {
    /// 把文本里出现的 key 换成 ***，用于错误信息和日志。
    pub fn redact(&self, s: &str) -> String {
        s.replace(&*self.key, "***")
    }

    /// 错误连同它的所有原因，去掉 key。reqwest 的顶层错误只有一句“error sending request”。
    pub fn describe(&self, e: &(dyn std::error::Error + 'static)) -> String {
        let mut parts = vec![e.to_string()];
        let mut cur = e.source();
        while let Some(c) = cur {
            let s = c.to_string();
            if !parts.iter().any(|p| p.contains(&s)) {
                parts.push(s);
            }
            cur = c.source();
        }
        self.redact(&parts.join("："))
    }
}
