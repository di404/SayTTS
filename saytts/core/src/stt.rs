//! 转写后端的接口。断句由本地断句器负责，所有后端共用：后端收到一句的音频帧和“一句说完”，
//! 交回中间结果和定稿。连接、重连、计费相关的策略归各后端自己管，见 `backend::*::stt`。

use tokio::sync::mpsc;

/// 管线内部的采样率。后端要别的采样率就自己转。
pub const STT_RATE: u32 = 16000;

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

/// 一个转写平台。
pub trait Stt: Send + Sync {
    /// 启动一个转写器。`Gate` 从返回的发送端进来，事件从 `events` 发出。
    /// 发送端全部丢掉时，转写器停止并关闭连接。`vocabulary` 是专有名词表。
    fn start(&self, vocabulary: Vec<String>, events: mpsc::UnboundedSender<SttEvent>) -> mpsc::UnboundedSender<Gate>;
}

/// 一个运行中的转写器。丢掉所有克隆时它就停止。
#[derive(Clone)]
pub struct Transcriber {
    tx: mpsc::UnboundedSender<Gate>,
}

impl Transcriber {
    pub fn start(stt: &dyn Stt, vocabulary: Vec<String>, events: mpsc::UnboundedSender<SttEvent>) -> Self {
        Self { tx: stt.start(vocabulary, events) }
    }

    pub fn send(&self, g: Gate) {
        let _ = self.tx.send(g);
    }
}
