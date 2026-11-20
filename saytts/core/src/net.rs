//! 各后端共用的 HTTP 客户端：普通请求可走 HTTP/2，WebSocket 必须 HTTP/1.1。
//! 三个客户端用同一个代理设置。代理为空时用系统代理（macOS 系统设置 / Windows 注册表 / 环境变量）。
//! 地址、鉴权头、错误里要遮掉的 key 都归各后端自己管。

use std::time::Duration;

use anyhow::{Context, Result};

#[derive(Clone)]
pub struct HttpKit {
    pub http: reqwest::Client,
    /// 不留空闲连接：每个请求新建一条连接。用于 TTS 的补发请求。
    pub fresh: reqwest::Client,
    pub ws: reqwest::Client,
}

impl HttpKit {
    /// `proxy` 形如 `http://127.0.0.1:7890` 或 `socks5h://127.0.0.1:7890`；空字符串表示用系统代理。
    pub fn new(proxy: &str) -> Result<Self> {
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
        Ok(Self { http: build(false, true)?, fresh: build(false, false)?, ws: build(true, true)? })
    }
}

/// 错误连同它的所有原因。reqwest 的顶层错误只有一句“error sending request”。
/// 不遮 key：调用方（后端）负责把自己的 key 换掉。
pub fn describe(e: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![e.to_string()];
    let mut cur = e.source();
    while let Some(c) = cur {
        let s = c.to_string();
        if !parts.iter().any(|p| p.contains(&s)) {
            parts.push(s);
        }
        cur = c.source();
    }
    parts.join("：")
}
