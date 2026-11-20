//! 本地断句：按 20 ms 一帧判断有没有在说话，静音满 500 ms 算一句说完。
//! 和 spike/stream_check.py 用的规则相同：门限 = max(300, 3 × 噪声底)，开口前补 300 ms。
//! 按键说话时不看音量：按住就送，松开就算说完。
//! 监听用外放时，麦克风会收进朗读的声音。开了“外放时暂停收音”，监听出声期间（`muted`）不送音频：正在说的一句马上结束，
//! 也不会开始新的一句，噪声底不受影响。
//! 自动断句时一句最长 `max_ms`：到时长后遇到 160 ms 的短停顿就切，这样不切在字中间；一直不停顿，再过 3 秒硬切。
//! 切开后如果还在说，下一帧马上开始下一句。`max_ms` 为 0 表示不限。

use std::collections::VecDeque;

use crate::stt::Gate;

pub const FRAME: usize = 320; // 20 ms @ 16 kHz
const FRAME_MS: u32 = 20;
const PREROLL: usize = 15; // 300 ms
const MIN_THRESHOLD: f32 = 300.0;
const FLOOR_WINDOW: usize = 1500; // 噪声底看最近 30 秒
const FLOOR_EVERY: usize = 25; // 每 0.5 秒重算一次
const BREATH_MS: u32 = 160; // 超长后，这么短的停顿也算一句
const OVERRUN_MS: u32 = 3000; // 超长后一直没有停顿，再过这么久硬切

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    PushToTalk,
}

pub struct Endpointer {
    pub mode: Mode,
    pub gate_ms: u32,
    pub max_ms: u32,
    pre: VecDeque<Vec<i16>>,
    speaking: bool,
    quiet_ms: u32,
    spoken_ms: u32,
    levels: VecDeque<f32>,
    since_floor: usize,
    floor: f32,
}

impl Endpointer {
    pub fn new(mode: Mode, gate_ms: u32, max_ms: u32) -> Self {
        Self {
            mode,
            gate_ms,
            max_ms,
            pre: VecDeque::with_capacity(PREROLL + 1),
            speaking: false,
            quiet_ms: 0,
            spoken_ms: 0,
            levels: VecDeque::with_capacity(FLOOR_WINDOW + 1),
            since_floor: 0,
            floor: 0.0,
        }
    }

    pub fn threshold(&self) -> f32 {
        (self.floor * 3.0).max(MIN_THRESHOLD)
    }

    pub fn speaking(&self) -> bool {
        self.speaking
    }

    /// 送进一帧（FRAME 个采样）。`held` 是按键说话的按键状态，自动断句时不用。
    /// 返回这一帧的音量（RMS）。
    /// `muted` 为真时这一帧丢掉（见模块说明）。
    pub fn push(&mut self, frame: &[i16], held: bool, muted: bool, out: &mut impl FnMut(Gate)) -> f32 {
        let level = rms(frame);
        if muted {
            self.flush(out);
            return level;
        }
        let voiced = match self.mode {
            Mode::Auto => level > self.threshold(),
            Mode::PushToTalk => held,
        };
        self.observe(level);

        if !self.speaking {
            self.pre.push_back(frame.to_vec());
            if self.pre.len() > PREROLL {
                self.pre.pop_front();
            }
            if voiced {
                self.speaking = true;
                self.quiet_ms = 0;
                self.spoken_ms = 0;
                for f in self.pre.drain(..) {
                    out(Gate::Frame(f));
                }
            }
            return level;
        }

        out(Gate::Frame(frame.to_vec()));
        let done = match self.mode {
            Mode::Auto => {
                self.quiet_ms = if voiced { 0 } else { self.quiet_ms + FRAME_MS };
                self.spoken_ms += FRAME_MS;
                let over = self.max_ms > 0 && self.spoken_ms >= self.max_ms;
                self.quiet_ms >= self.gate_ms
                    || (over && self.quiet_ms >= BREATH_MS.min(self.gate_ms))
                    || (over && self.spoken_ms >= self.max_ms + OVERRUN_MS)
            }
            Mode::PushToTalk => !held,
        };
        if done {
            self.speaking = false;
            out(Gate::End);
        }
        level
    }

    /// 停止时调用：正在说的一句马上结束。
    pub fn flush(&mut self, out: &mut impl FnMut(Gate)) {
        if self.speaking {
            self.speaking = false;
            out(Gate::End);
        }
        self.pre.clear();
    }

    fn observe(&mut self, level: f32) {
        self.levels.push_back(level);
        if self.levels.len() > FLOOR_WINDOW {
            self.levels.pop_front();
        }
        self.since_floor += 1;
        if self.since_floor >= FLOOR_EVERY && self.levels.len() >= 50 {
            self.since_floor = 0;
            let mut v: Vec<f32> = self.levels.iter().copied().collect();
            let k = v.len() / 5;
            v.select_nth_unstable_by(k, f32::total_cmp);
            self.floor = v[k];
        }
    }
}

pub fn rms(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f64 = frame.iter().map(|&v| (v as f64) * (v as f64)).sum();
    (sum / frame.len() as f64).sqrt() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(amp: i16) -> Vec<i16> {
        (0..FRAME).map(|i| if i % 2 == 0 { amp } else { -amp }).collect()
    }

    fn feed(ep: &mut Endpointer, ev: &mut Vec<Gate>, amp: i16, held: bool, n: usize) {
        for _ in 0..n {
            ep.push(&tone(amp), held, false, &mut |g| ev.push(g));
        }
    }

    #[test]
    fn splits_on_500ms_of_silence() {
        let mut ep = Endpointer::new(Mode::Auto, 500, 0);
        let mut ev = Vec::new();
        feed(&mut ep, &mut ev, 10, false, 30); // 静音
        feed(&mut ep, &mut ev, 3000, false, 50); // 1 秒说话
        feed(&mut ep, &mut ev, 10, false, 24); // 480 ms 静音：还没断
        assert!(!ev.contains(&Gate::End));
        feed(&mut ep, &mut ev, 10, false, 1); // 500 ms
        let frames = ev.iter().filter(|g| matches!(g, Gate::Frame(_))).count();
        assert_eq!(ev.last(), Some(&Gate::End));
        assert_eq!(frames, PREROLL + 49 + 25); // 开口前 300 ms + 说话 + 尾部静音
    }

    #[test]
    fn push_to_talk_follows_the_key() {
        let mut ep = Endpointer::new(Mode::PushToTalk, 500, 0);
        let mut ev = Vec::new();
        feed(&mut ep, &mut ev, 5000, false, 20); // 没按键：不送
        assert!(ev.is_empty());
        feed(&mut ep, &mut ev, 10, true, 10);
        feed(&mut ep, &mut ev, 10, false, 1);
        assert_eq!(ev.last(), Some(&Gate::End));
        assert_eq!(ev.len(), PREROLL + 9 + 1 + 1);
    }

    #[test]
    fn muted_frames_end_the_utterance_and_never_start_one() {
        let mut ep = Endpointer::new(Mode::Auto, 500, 0);
        let mut ev = Vec::new();
        feed(&mut ep, &mut ev, 10, false, 60);
        let floor = ep.threshold();
        feed(&mut ep, &mut ev, 3000, false, 10); // 开口
        assert!(ep.speaking());
        ep.push(&tone(3000), false, true, &mut |g| ev.push(g)); // 监听开始出声
        assert_eq!(ev.last(), Some(&Gate::End));
        assert!(!ep.speaking());
        let n = ev.len();
        for _ in 0..200 {
            ep.push(&tone(3000), false, true, &mut |g| ev.push(g)); // 回声
        }
        assert_eq!(ev.len(), n);
        assert_eq!(ep.threshold(), floor); // 回声不抬高门限
        feed(&mut ep, &mut ev, 10, false, 1);
        feed(&mut ep, &mut ev, 3000, false, 1); // 监听停了再开口：开口前补的只有静音
        let pre: Vec<_> = ev[n..].iter().collect();
        assert_eq!(pre.len(), 2);
    }

    #[test]
    fn long_speech_splits_at_a_short_pause_after_max() {
        let mut ep = Endpointer::new(Mode::Auto, 500, 2000);
        let mut ev = Vec::new();
        feed(&mut ep, &mut ev, 10, false, 30);
        feed(&mut ep, &mut ev, 3000, false, 50); // 1 秒
        feed(&mut ep, &mut ev, 10, false, 10); // 200 ms 停顿：没到时长，不切
        feed(&mut ep, &mut ev, 3000, false, 50); // 到 2.2 秒
        assert!(!ev.contains(&Gate::End));
        feed(&mut ep, &mut ev, 10, false, 7); // 140 ms：还不够
        assert!(!ev.contains(&Gate::End));
        feed(&mut ep, &mut ev, 10, false, 1); // 160 ms：切
        assert_eq!(ev.last(), Some(&Gate::End));
        assert!(!ep.speaking());
    }

    #[test]
    fn nonstop_speech_is_cut_and_continues() {
        let mut ep = Endpointer::new(Mode::Auto, 500, 2000);
        let mut ev = Vec::new();
        feed(&mut ep, &mut ev, 10, false, 1000); // 噪声底按最低 20% 算，静音要够多，后面一直说才不会抬高门限
        feed(&mut ep, &mut ev, 3000, false, 1);
        let first = ev.len();
        feed(&mut ep, &mut ev, 3000, false, 249); // 开口后 4.98 秒：还没切
        assert!(!ev.contains(&Gate::End));
        feed(&mut ep, &mut ev, 3000, false, 1); // 5 秒 = 2 秒 + 3 秒：硬切
        assert_eq!(ev.last(), Some(&Gate::End));
        let n = ev.len();
        feed(&mut ep, &mut ev, 3000, false, 1); // 接着说：马上开始下一句，不重复送已经送过的音频
        assert!(ep.speaking());
        assert_eq!(ev.len(), n + 1);
        assert_eq!(n - first, 250 + 1); // 说话帧 + End
    }
}
