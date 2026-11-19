//! 不开界面、不出声地测试云端链路。key 读环境变量 FISH_API_KEY；代理默认用系统代理。
//!
//!   probe tts  [--model M] [--voice V] [--runs N] [--gap 秒] [--text T] [--out DIR]
//!   probe stt  --wav F [--repeat N] [--gap 秒]
//!   probe voices
//!   probe devices [--open 1]
//!   probe e2e  --wav F [--repeat N] [--gap 秒] [--model M] [--voice V]
//!   (公共) [--proxy URL]

use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use voice_core::{
    audio,
    endpointer::{Endpointer, FRAME, Mode},
    pipeline::{Config, Event, Input, Pipeline, Sink},
    queue::VoiceConfig,
    net::Net,
    stt::{Gate, STT_RATE, SttEvent, Transcriber},
    tts::{self, TTS_RATE},
    wav,
};

struct Args {
    cmd: String,
    opts: HashMap<String, String>,
}

impl Args {
    fn parse() -> Self {
        let mut it = std::env::args().skip(1);
        let cmd = it.next().unwrap_or_default();
        let mut opts = HashMap::new();
        while let Some(k) = it.next() {
            if let Some(k) = k.strip_prefix("--") {
                opts.insert(k.to_string(), it.next().unwrap_or_default());
            }
        }
        Self { cmd, opts }
    }
    fn get(&self, k: &str, default: &str) -> String {
        self.opts.get(k).cloned().unwrap_or_else(|| default.to_string())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let key = std::env::var("FISH_API_KEY").context("需要环境变量 FISH_API_KEY")?;
    let net = Net::new(&key, &args.get("proxy", ""))?;
    match args.cmd.as_str() {
        "tts" => tts_cmd(&net, &args).await,
        "stt" => stt_cmd(&net, &args).await,
        "voices" => voices_cmd(&net).await,
        "devices" => devices_cmd(&args),
        "e2e" => e2e_cmd(&net, &args).await,
        _ => {
            eprintln!("用法：probe tts|stt|voices [选项]");
            Ok(())
        }
    }
}

async fn tts_cmd(net: &Net, args: &Args) -> Result<()> {
    let model = args.get("model", tts::MODELS[0]);
    let voice = args.get("voice", voice_core::voices::VOICES[0].id);
    let text = args.get("text", "今天我们来玩一下这个新出的解谜游戏，看看能不能一次通关。");
    let runs: usize = args.get("runs", "3").parse()?;
    let gap: f64 = args.get("gap", "0").parse()?;
    let out = PathBuf::from(args.get("out", "target/probe"));
    let mut firsts = Vec::new();
    let mut sounds = Vec::new();
    for run in 0..runs {
        if run > 0 && gap > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(gap)).await; // 模拟两句之间连接空着
        }
        let t0 = Instant::now();
        let mut first = None;
        let mut sound = None;
        let mut pcm: Vec<i16> = Vec::new();
        let mut chunks: Vec<(Duration, usize)> = Vec::new(); // 每块到达的时刻、它在整句里的起点
        let mut keep = 0; // app 从这个采样开始播：第一个有声采样前留 10 ms
        let synth = tts::synthesize(net, &model, &voice, &text, |chunk| {
            first.get_or_insert(t0.elapsed());
            chunks.push((t0.elapsed(), pcm.len()));
            if sound.is_none() {
                if let Some(i) = chunk.iter().position(|v| v.unsigned_abs() > 300) {
                    // 第一个有声采样在音频里的位置也算进去：它要等前面的静音播完才出声
                    let lead = Duration::from_secs_f64((pcm.len() + i) as f64 / TTS_RATE as f64);
                    sound = Some((t0.elapsed(), lead));
                    keep = pcm.len() + i.saturating_sub(240);
                }
            }
            pcm.extend_from_slice(&chunk);
        });
        match tokio::time::timeout(Duration::from_secs(30), synth).await {
            Ok(r) => r?,
            Err(_) => {
                println!("#{run}: 30 秒没合成完（首块 {:?}）", first.map(ms));
                continue;
            }
        }
        let total = t0.elapsed();
        let audio = pcm.len() as f64 / TTS_RATE as f64;
        let (st, lead) = sound.unwrap_or_default();
        println!(
            "#{run} {model} {voice}: 首块 {} ms，首个有声采样到达 {} ms（音频内静音 {} ms），合成完 {} ms，音频 {:.2} s，首块之后 {:.1}× 实时",
            ms(first.unwrap_or_default()),
            ms(st),
            ms(lead),
            ms(total),
            audio,
            // 低于 1 时，从首块开始播放会在句子中途断音
            audio / (total - first.unwrap_or_default()).as_secs_f64()
        );
        // 和 app 一样，首个有声块一到就开始播。之后每块最晚要在播到它的时刻到达，晚多少，播放就停多少。
        // 最大的那个差，就是要先攒多少音频再开始播才不断音。
        let need = chunks
            .iter()
            .filter(|(_, p)| *p > keep)
            .map(|(a, p)| (*a - st).as_secs_f64() - (p - keep) as f64 / TTS_RATE as f64)
            .fold(0.0, f64::max);
        let tail = total - chunks.last().map_or(total, |c| c.0);
        println!(
            "    {} 块；先攒 {} ms 才不断音；最后一块音频到流结束 {} ms",
            chunks.len(),
            (need * 1000.0).round(),
            ms(tail)
        );
        firsts.push(ms(first.unwrap_or_default()));
        sounds.push(ms(st));
        if run + 1 == runs {
            let path = out.join(format!("{model}-{voice}.wav"));
            wav::write_mono(&path, &pcm, TTS_RATE)?;
            println!("样本：{}", path.display());
        }
    }
    println!("中位数：首块 {} ms，首个有声采样 {} ms", median(&mut firsts), median(&mut sounds));
    Ok(())
}

async fn stt_cmd(net: &Net, args: &Args) -> Result<()> {
    let path = PathBuf::from(args.get("wav", ""));
    let pcm = wav::read_mono(&path, STT_RATE).with_context(|| format!("读不了 {}", path.display()))?;
    let repeat: usize = args.get("repeat", "1").parse()?;
    let gap: f64 = args.get("gap", "2").parse()?;
    let (etx, mut erx) = mpsc::unbounded_channel();
    let t0 = Instant::now();
    let tr = Transcriber::start(net.clone(), vocab(args), etx);

    // 先等连接好，记下建连时间
    match tokio::time::timeout(Duration::from_secs(20), erx.recv()).await {
        Ok(Some(SttEvent::Connected)) => println!("[{:6} ms] 已连接", ms(t0.elapsed())),
        other => anyhow::bail!("连接失败：{other:?}"),
    }

    let (end_tx, mut end_rx) = mpsc::unbounded_channel::<Instant>();
    let clip_secs = pcm.len() as f64 / STT_RATE as f64;
    let feeder = {
        let tr = tr.clone();
        tokio::task::spawn_blocking(move || {
            let mut ep = Endpointer::new(Mode::Auto, 500, 0);
            let silence = vec![0i16; (gap * STT_RATE as f64) as usize];
            let start = Instant::now();
            let mut n = 0usize;
            for r in 0..repeat {
                let mut seq: Vec<&[i16]> = vec![&pcm];
                if r + 1 < repeat {
                    seq.push(&silence);
                }
                for part in seq {
                    for f in part.chunks_exact(FRAME) {
                        n += 1;
                        let due = start + Duration::from_millis(20 * n as u64);
                        if let Some(d) = due.checked_duration_since(Instant::now()) {
                            std::thread::sleep(d);
                        }
                        ep.push(f, false, false, &mut |g| {
                            if g == Gate::End {
                                let _ = end_tx.send(Instant::now());
                            }
                            tr.send(g);
                        });
                    }
                }
            }
            // 结尾补 1 秒静音，让最后一句也能断开
            for _ in 0..50 {
                n += 1;
                let due = start + Duration::from_millis(20 * n as u64);
                if let Some(d) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
                ep.push(&[0; FRAME], false, false, &mut |g| {
                    if g == Gate::End {
                        let _ = end_tx.send(Instant::now());
                    }
                    tr.send(g);
                });
            }
        })
    };

    let mut ends: Vec<Instant> = Vec::new();
    let mut finals = 0usize;
    let mut lat = Vec::new();
    let deadline = Instant::now() + Duration::from_secs_f64((clip_secs + gap) * repeat as f64 + 15.0);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        tokio::select! {
            Some(t) = end_rx.recv() => {
                ends.push(t);
                println!("[{:6} ms] 说完（本地断句）", ms(t0.elapsed()));
            }
            ev = erx.recv() => {
                let Some(ev) = ev else { break };
                let now = Instant::now();
                match &ev {
                    SttEvent::Final(t) => {
                        let since = ends.get(finals).map(|e| ms(now - *e));
                        finals += 1;
                        if let Some(s) = since { lat.push(s); }
                        println!("[{:6} ms] 定稿（说完后 {} ms）：{t}", ms(t0.elapsed()), since.map_or("?".into(), |s| s.to_string()));
                        if finals >= repeat { break; }
                    }
                    SttEvent::Interim(t) => println!("[{:6} ms] 中间结果：{t}", ms(t0.elapsed())),
                    other => println!("[{:6} ms] {other:?}", ms(t0.elapsed())),
                }
            }
            _ = tokio::time::sleep(left) => { println!("超时"); break; }
        }
    }
    feeder.await?;
    if !lat.is_empty() {
        println!("说完到定稿：中位数 {} ms（{} 句）", median(&mut lat), lat.len());
    }
    Ok(())
}

async fn voices_cmd(net: &Net) -> Result<()> {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for chunk in voice_core_voices().chunks(6) {
        let rs = futures_util::future::join_all(chunk.iter().map(|v| async move {
            (v, tts::synthesize(net, tts::MODELS[0], v, "好的。", |_| {}).await)
        }))
        .await;
        for (v, r) in rs {
            match r {
                Ok(()) => ok.push(*v),
                Err(e) => bad.push(format!("{v}: {e:#}")),
            }
        }
    }
    println!("可用 {}：{}", ok.len(), ok.join(", "));
    for b in bad {
        println!("不可用 {b}");
    }
    Ok(())
}

fn devices_cmd(args: &Args) -> Result<()> {
    for (label, list) in [("输入", audio::inputs()?), ("输出", audio::outputs()?)] {
        println!("{label}设备：");
        for d in &list {
            println!("  {} {}", if d.default { "*" } else { " " }, d.name);
        }
    }
    if args.get("open", "0") == "1" {
        // 只打开不送音频：输出的是静音
        let outs = audio::outputs()?;
        let pick = outs.iter().find(|d| d.name.contains("BlackHole")).or_else(|| outs.iter().find(|d| d.default));
        if let Some(d) = pick {
            let o = audio::Output::open(&d.id, |e| eprintln!("{e}"))?;
            std::thread::sleep(Duration::from_millis(500));
            println!("已打开输出“{}”，{} Hz，0.5 秒内播出静音 {} 个采样", o.name, o.rate, o.played());
        }
    }
    Ok(())
}

async fn e2e_cmd(net: &Net, args: &Args) -> Result<()> {
    let wav_in = PathBuf::from(args.get("wav", ""));
    let repeat: usize = args.get("repeat", "1").parse()?;
    let gap: f64 = args.get("gap", "3").parse()?;
    let gate_ms = 500u32;
    // 把同一段录音重复几遍，中间插静音，存成一个临时 WAV 当输入
    let pcm = wav::read_mono(&wav_in, STT_RATE)?;
    let mut seq = Vec::new();
    for r in 0..repeat {
        seq.extend_from_slice(&pcm);
        if r + 1 < repeat {
            seq.extend(std::iter::repeat_n(0i16, (gap * STT_RATE as f64) as usize));
        }
    }
    let out_dir = PathBuf::from(args.get("out", "target/probe"));
    let input = out_dir.join("e2e-in.wav");
    wav::write_mono(&input, &seq, STT_RATE)?;
    let cfg = Config {
        input: Input::Wav(input),
        output: Sink::Wav(out_dir.join("e2e-out.wav")),
        monitor: None,
        mode: Mode::Auto,
        gate_ms,
        max_ms: 0,
        voice: VoiceConfig { model: args.get("model", tts::MODELS[0]), voice: args.get("voice", voice_core::voices::VOICES[0].id) },
        vocabulary: vocab(args),
        monitor_volume: 1.0,
        echo_gate: false,
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let t0 = Instant::now();
    let p = Pipeline::start(net.clone(), cfg, Default::default(), tx)?;
    let mut ends = Vec::new();
    let mut finals = Vec::new();
    let mut plays = Vec::new();
    let mut done = 0;
    let deadline = Instant::now() + Duration::from_secs_f64(seq.len() as f64 / STT_RATE as f64 + 30.0);
    while done < repeat {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(Some(ev)) = tokio::time::timeout(left, rx.recv()).await else {
            println!("超时");
            break;
        };
        let now = Instant::now();
        let at = ms(now - t0);
        match &ev {
            Event::Level { .. } | Event::Interim { .. } => continue,
            Event::SpeechEnd => ends.push(now),
            Event::Final { .. } => finals.push(now),
            Event::Playing { .. } => plays.push(now),
            Event::Done { .. } => done += 1,
            _ => {}
        }
        println!("[{at:6} ms] {}", serde_json::to_string(&ev)?);
    }
    drop(p);
    println!();
    let gate = Duration::from_millis(gate_ms as u64);
    let mut e2e = Vec::new();
    for (i, e) in ends.iter().enumerate() {
        let (Some(f), Some(pl)) = (finals.get(i), plays.get(i)) else { continue };
        let total = ms(*pl - *e + gate);
        e2e.push(total);
        println!(
            "第 {} 句：静音判定 {} ms → 定稿 +{} ms → 出声 +{} ms；说完到出声 {} ms",
            i + 1,
            gate_ms,
            ms(*f - *e),
            ms(*pl - *f),
            total
        );
    }
    if !e2e.is_empty() {
        println!("说完到出声：中位数 {} ms（{} 句）", median(&mut e2e), e2e.len());
    }
    Ok(())
}

fn voice_core_voices() -> Vec<&'static str> {
    voice_core::voices::VOICES.iter().map(|v| v.id).collect()
}

fn ms(d: Duration) -> u128 {
    d.as_millis()
}

fn median(v: &mut [u128]) -> u128 {
    if v.is_empty() {
        return 0;
    }
    v.sort();
    v[v.len() / 2]
}

/// `--vocab 会话,弹幕`
fn vocab(args: &Args) -> Vec<String> {
    args.get("vocab", "").split([',', '，']).map(str::trim).filter(|w| !w.is_empty()).map(String::from).collect()
}
