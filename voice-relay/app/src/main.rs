#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 桌面外壳：设置、API key、按键说话快捷键、OBS 字幕页。
//! 采集、转写、朗读、播放都在 voice-core 里，界面只负责显示和改设置。

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Listener, Manager, State, webview::PageLoadEvent};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tokio::sync::mpsc;
use voice_core::{
    audio::{self, DeviceInfo},
    endpointer::Mode,
    net::Net,
    overlay::Overlay,
    pipeline::{Config, Event, Input, Pipeline, Sink},
    queue::{self, VoiceConfig},
    tts,
    voices::{self, OwnVoice, VOICES, Voice},
};

/// 旧版本把 key 存在系统钥匙串里。只在第一次启动新版本时读一次，搬到配置目录后删掉。
const KEY_SERVICE: &str = "voice-relay";
const KEY_ACCOUNT: &str = "fish-api-key";
/// key 和 settings.json 放在同一个目录，但单独一个文件：快照会把设置发给界面，key 不能跟着发过去。
const KEY_FILE: &str = "fish-api-key";
const PREVIEW_TEXT: &str = "大家好，这是现在选中的声音。";
/// 一句播完后，字幕再留这么久。
const LINGER: std::time::Duration = std::time::Duration::from_millis(1500);
/// 运行中多久检查一次设备：设备断开、或用“系统默认”时默认设备换了，就重新打开。
const DEVICE_CHECK: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct Settings {
    input: String,
    output: String,
    /// 空字符串表示不监听。
    monitor: String,
    /// 0.0–1.0
    monitor_volume: f32,
    /// 主界面上临时关掉监听。只把监听音量压到 0，不换设备，所以不用重启链路。
    monitor_muted: bool,
    /// 监听出声时不收音。监听用外放时防止朗读被麦克风收进去，代价是这段时间说的话会丢。
    echo_gate: bool,
    /// "auto" 或 "ptt"
    mode: String,
    ptt_key: String,
    gate_ms: u32,
    /// 自动断句时一句最长多少秒，超过就在下一个短停顿处切开
    max_s: u32,
    model: String,
    /// Fish Audio 声音的 reference_id
    voice: String,
    /// 专有名词表
    vocabulary: Vec<String>,
    proxy: String,
    overlay_port: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            input: String::new(),
            output: String::new(),
            monitor: String::new(),
            monitor_volume: 1.0,
            monitor_muted: false,
            echo_gate: false,
            mode: "auto".into(),
            ptt_key: "F8".into(),
            gate_ms: 500,
            max_s: 10,
            model: tts::MODELS[0].into(),
            voice: VOICES[0].id.into(),
            vocabulary: Vec::new(),
            proxy: String::new(),
            overlay_port: 7788,
        }
    }
}

impl Settings {
    fn voice_config(&self) -> VoiceConfig {
        VoiceConfig { model: self.model.clone(), voice: self.voice.clone() }
    }
    /// 监听实际用的音量
    fn monitor_gain(&self) -> f32 {
        if self.monitor_muted { 0.0 } else { self.monitor_volume }
    }
    /// 改了这些要重启链路；只改声音或监听音量不用。
    fn audio_part(&self) -> (&str, &str, &str, bool, &str, u32, u32, &[String], &str) {
        (&self.input, &self.output, &self.monitor, self.echo_gate, &self.mode, self.gate_ms, self.max_s, &self.vocabulary, &self.proxy)
    }
}

struct AppState {
    path: PathBuf,
    settings: Mutex<Settings>,
    pipeline: Mutex<Option<Pipeline>>,
    /// 用户点了开始、还没点停止。设备断开导致链路停了也保持为真，设备回来后自动重开。
    want: AtomicBool,
    /// 当前这次运行的麦克风或输出设备报告断开。
    device_lost: AtomicBool,
    /// 这次运行开始时的系统默认（麦克风, 输出）。
    defaults: Mutex<(String, String)>,
    /// 想运行但打不开设备时的原因。
    waiting: Mutex<Option<String>>,
    /// 每次开始加一。换设置重启时，上一次的事件可能晚到，界面和字幕页只认当前这次。
    run: AtomicU64,
    held: Arc<AtomicBool>,
    key_path: PathBuf,
    /// 保存的 key 优先；没有就用环境变量 FISH_API_KEY。
    key: Mutex<Option<(String, &'static str)>>,
    overlay: Mutex<Option<Overlay>>,
    overlay_error: Mutex<Option<String>>,
    hotkey: Mutex<Option<Shortcut>>,
    hotkey_error: Mutex<Option<String>>,
}

#[derive(Serialize)]
struct Snapshot {
    settings: Settings,
    key_source: Option<&'static str>,
    running: bool,
    want: bool,
    waiting: Option<String>,
    overlay_url: Option<String>,
    overlay_error: Option<String>,
    hotkey_error: Option<String>,
    inputs: Vec<DeviceInfo>,
    outputs: Vec<DeviceInfo>,
    voices: Vec<Voice>,
    models: Vec<&'static str>,
}

fn snapshot_of(s: &AppState) -> Snapshot {
    Snapshot {
        settings: s.settings.lock().unwrap().clone(),
        key_source: s.key.lock().unwrap().as_ref().map(|(_, src)| *src),
        running: s.pipeline.lock().unwrap().is_some(),
        want: s.want.load(Ordering::Acquire),
        waiting: s.waiting.lock().unwrap().clone(),
        overlay_url: s.overlay.lock().unwrap().as_ref().map(Overlay::url),
        overlay_error: s.overlay_error.lock().unwrap().clone(),
        hotkey_error: s.hotkey_error.lock().unwrap().clone(),
        inputs: audio::inputs().unwrap_or_default(),
        outputs: audio::outputs().unwrap_or_default(),
        voices: VOICES.to_vec(),
        models: tts::MODELS.to_vec(),
    }
}

fn env_key() -> Option<(String, &'static str)> {
    std::env::var("FISH_API_KEY").ok().filter(|k| !k.trim().is_empty()).map(|k| (k, "env"))
}

fn write_key(path: &std::path::Path, key: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, key)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// 启动时读一次 key。配置目录里没有、钥匙串里有（旧版本存的），就搬过来并删掉钥匙串里那条，
/// 以后启动不再访问钥匙串（未签名的 app 每次重新编译后访问钥匙串都会弹窗）。
/// `from_keychain` 为 false 时不碰钥匙串（测试模式的配置目录是临时目录）。
fn load_key(path: &std::path::Path, from_keychain: bool) -> Option<(String, &'static str)> {
    if let Ok(k) = std::fs::read_to_string(path) {
        let k = k.trim();
        if !k.is_empty() {
            return Some((k.to_string(), "saved"));
        }
    }
    if let Some(entry) = keyring::Entry::new(KEY_SERVICE, KEY_ACCOUNT).ok().filter(|_| from_keychain)
        && let Ok(k) = entry.get_password()
    {
        let k = k.trim().to_string();
        if !k.is_empty() && write_key(path, &k).is_ok() {
            let _ = entry.delete_credential();
            return Some((k, "saved"));
        }
    }
    env_key()
}

fn net(s: &AppState) -> Result<Net, String> {
    let key = s.key.lock().unwrap().as_ref().map(|(k, _)| k.clone()).ok_or("请先填写 Fish Audio API key")?;
    let proxy = s.settings.lock().unwrap().proxy.clone();
    Net::new(&key, &proxy).map_err(|e| format!("{e:#}"))
}

fn save_file(s: &AppState) -> Result<(), String> {
    let json = serde_json::to_string_pretty(&*s.settings.lock().unwrap()).map_err(|e| e.to_string())?;
    if let Some(dir) = s.path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&s.path, json).map_err(|e| format!("保存设置失败：{e}"))
}

/// 按设置注册（或取消）按键说话的全局快捷键。
fn register_hotkey(app: &AppHandle) {
    let s = app.state::<AppState>();
    let gs = app.global_shortcut();
    if let Some(old) = s.hotkey.lock().unwrap().take() {
        let _ = gs.unregister(old);
    }
    s.held.store(false, Ordering::Release);
    let settings = s.settings.lock().unwrap().clone();
    let result = if settings.mode != "ptt" {
        Ok(None)
    } else {
        settings
            .ptt_key
            .trim()
            .parse::<Shortcut>()
            .map_err(|e| format!("快捷键无效：{e}"))
            .and_then(|sc| {
                let (held, handle) = (s.held.clone(), app.clone());
                gs.on_shortcut(sc, move |_, _, ev| {
                    let down = ev.state == ShortcutState::Pressed;
                    held.store(down, Ordering::Release);
                    let _ = handle.emit("ptt", down);
                })
                .map(|_| Some(sc))
                .map_err(|e| format!("快捷键注册失败：{e}"))
            })
    };
    match result {
        Ok(sc) => {
            *s.hotkey.lock().unwrap() = sc;
            *s.hotkey_error.lock().unwrap() = None;
        }
        Err(e) => *s.hotkey_error.lock().unwrap() = Some(e),
    }
}

fn start_pipeline(app: &AppHandle) -> Result<(), String> {
    let s = app.state::<AppState>();
    let net = net(&s)?;
    let cfg = {
        let st = s.settings.lock().unwrap();
        Config {
            // 开发测试用（只在调试版里有）：按实时节奏读这个 WAV 当麦克风，不打开真的麦克风
            input: match std::env::var_os("VOICE_RELAY_TEST_WAV").filter(|_| cfg!(debug_assertions)) {
                Some(path) => Input::Wav(path.into()),
                None => Input::Mic(st.input.clone()),
            },
            output: Sink::Device(st.output.clone()),
            monitor: (!st.monitor.is_empty()).then(|| Sink::Device(st.monitor.clone())),
            mode: if st.mode == "ptt" { Mode::PushToTalk } else { Mode::Auto },
            gate_ms: st.gate_ms.clamp(200, 2000),
            max_ms: st.max_s.clamp(3, 60) * 1000,
            voice: st.voice_config(),
            vocabulary: st.vocabulary.clone(),
            monitor_volume: st.monitor_gain(),
            echo_gate: st.echo_gate,
        }
    };
    let defaults = (audio::default_id(true), audio::default_id(false));
    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    let pipeline = Pipeline::start(net, cfg, s.held.clone(), tx).map_err(|e| format!("{e:#}"))?;
    let run = s.run.fetch_add(1, Ordering::AcqRel) + 1;
    s.device_lost.store(false, Ordering::Release);
    *s.defaults.lock().unwrap() = defaults;
    *s.waiting.lock().unwrap() = None;
    *s.pipeline.lock().unwrap() = Some(pipeline);

    // 事件：字幕页跟着播放进度显示、隐藏；全部带上 run 转发给界面
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        // 句子编号每次开始都从 1 起；字幕页用的编号加上 run，前后两次不会撞
        let key = |id: u64| run << 32 | id;
        while let Some(ev) = rx.recv().await {
            let st = handle.state::<AppState>();
            let current = st.run.load(Ordering::Acquire) == run;
            if current && matches!(ev, Event::DeviceLost { .. }) {
                st.device_lost.store(true, Ordering::Release);
            }
            let overlay = if current { st.overlay.lock().unwrap().clone() } else { None };
            if let Some(o) = overlay {
                match &ev {
                    Event::Playing { id, text } => o.show(key(*id), text),
                    // 下一句已经显示时，hide 不起作用
                    Event::Done { id, .. } => {
                        let id = key(*id);
                        tauri::async_runtime::spawn(async move {
                            tokio::time::sleep(LINGER).await;
                            o.hide(id);
                        });
                    }
                    Event::Cleared => o.clear(),
                    _ => {}
                }
            }
            if let Ok(mut v) = serde_json::to_value(&ev) {
                v["run"] = run.into();
                let _ = handle.emit("pipeline", v);
            }
        }
    });
    Ok(())
}

fn stop_pipeline(s: &AppState) {
    let p = s.pipeline.lock().unwrap().take();
    drop(p); // 停采集、关转写连接、停播放
    if let Some(o) = s.overlay.lock().unwrap().as_ref() {
        o.clear();
    }
}

/// 设备断开、或麦克风用“系统默认”时默认麦克风换了：重新打开链路。打不开就记下原因，下次检查再试。
/// 输出用“系统默认”时不用管：cpal 打开的默认输出会自己跟着系统切换。
fn device_check(app: &AppHandle) {
    let s = app.state::<AppState>();
    if !s.want.load(Ordering::Acquire) {
        return;
    }
    let running = s.pipeline.lock().unwrap().is_some();
    let need = !running || s.device_lost.load(Ordering::Acquire) || {
        let follow_default = s.settings.lock().unwrap().input.is_empty();
        follow_default && audio::default_id(true) != s.defaults.lock().unwrap().0
    };
    if !need {
        return;
    }
    let before = s.waiting.lock().unwrap().clone();
    stop_pipeline(&s);
    let now = match start_pipeline(app) {
        Ok(()) => {
            // 重开期间用户点了停止
            if !s.want.load(Ordering::Acquire) {
                stop_pipeline(&s);
            }
            None
        }
        Err(e) => Some(e),
    };
    *s.waiting.lock().unwrap() = now.clone();
    // 只在等待状态变了时通知界面：每 2 秒刷新一次会打断正在设置里选设备的用户
    if before != now {
        let _ = app.emit("state", ());
    }
}

/// 列设备要问 CoreAudio，可能要几百毫秒，不放在主线程上。
#[tauri::command]
async fn snapshot(app: AppHandle) -> Snapshot {
    snapshot_of(&app.state::<AppState>())
}

#[tauri::command]
async fn save_settings(app: AppHandle, settings: Settings) -> Result<Snapshot, String> {
    let s = app.state::<AppState>();
    let old = s.settings.lock().unwrap().clone();
    *s.settings.lock().unwrap() = settings.clone();
    save_file(&s)?;
    if old.mode != settings.mode || old.ptt_key != settings.ptt_key {
        register_hotkey(&app);
    }
    if old.overlay_port != settings.overlay_port {
        start_overlay(&app).await;
    }
    let running = s.pipeline.lock().unwrap().is_some();
    if running {
        if old.audio_part() != settings.audio_part() {
            stop_pipeline(&s);
            start_pipeline(&app)?;
        } else if let Some(p) = s.pipeline.lock().unwrap().as_ref() {
            if old.voice_config() != settings.voice_config() {
                p.set_voice(settings.voice_config());
            }
            if old.monitor_gain() != settings.monitor_gain() {
                p.set_monitor_volume(settings.monitor_gain());
            }
        }
    }
    Ok(snapshot_of(&s))
}

#[tauri::command]
fn set_api_key(state: State<'_, AppState>, key: String) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("key 为空".into());
    }
    write_key(&state.key_path, key).map_err(|e| format!("保存 key 失败：{e}"))?;
    *state.key.lock().unwrap() = Some((key.to_string(), "saved"));
    Ok(())
}

#[tauri::command]
fn clear_api_key(state: State<'_, AppState>) -> Result<(), String> {
    match std::fs::remove_file(&state.key_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("清除 key 失败：{e}")),
    }
    *state.key.lock().unwrap() = env_key();
    Ok(())
}

#[tauri::command]
async fn start(app: AppHandle) -> Result<(), String> {
    let s = app.state::<AppState>();
    if s.pipeline.lock().unwrap().is_some() {
        return Ok(());
    }
    start_pipeline(&app)?;
    s.want.store(true, Ordering::Release);
    Ok(())
}

#[tauri::command]
fn stop(state: State<'_, AppState>) {
    state.want.store(false, Ordering::Release);
    *state.waiting.lock().unwrap() = None;
    stop_pipeline(&state);
}

/// 打字输入，或重试读失败的句子。
#[tauri::command]
fn say(state: State<'_, AppState>, text: String) -> Result<(), String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(());
    }
    match state.pipeline.lock().unwrap().as_ref() {
        Some(p) => {
            p.say(text.to_string());
            Ok(())
        }
        None => Err("先点开始".into()),
    }
}

#[tauri::command]
fn stop_speech(state: State<'_, AppState>) {
    if let Some(p) = state.pipeline.lock().unwrap().as_ref() {
        p.stop_speech();
    }
}

/// 账号里自己的声音。
#[tauri::command]
async fn own_voices(app: AppHandle) -> Result<Vec<OwnVoice>, String> {
    let net = net(&app.state::<AppState>())?;
    voices::own_voices(&net).await.map_err(|e| format!("{e:#}"))
}

/// 试听：用选中的声音读一句，放到监听设备；没选监听设备时放到系统默认输出。
#[tauri::command]
async fn preview(app: AppHandle, voice: VoiceConfig) -> Result<(), String> {
    let s = app.state::<AppState>();
    let net = net(&s)?;
    let (device, volume) = {
        let st = s.settings.lock().unwrap();
        (st.monitor.clone(), if st.monitor.is_empty() { 1.0 } else { st.monitor_volume })
    };
    queue::preview(&net, &voice, &device, volume, PREVIEW_TEXT).await.map_err(|e| format!("{e:#}"))
}

async fn start_overlay(app: &AppHandle) {
    let s = app.state::<AppState>();
    let port = s.settings.lock().unwrap().overlay_port;
    if s.overlay.lock().unwrap().as_ref().is_some_and(|o| o.port == port) {
        return;
    }
    // 先占新端口，成功了再关旧的：新端口被占用时旧地址还能用
    match Overlay::start(port).await {
        Ok(o) => {
            if let Some(old) = s.overlay.lock().unwrap().replace(o) {
                old.shutdown();
            }
            *s.overlay_error.lock().unwrap() = None;
        }
        Err(e) => *s.overlay_error.lock().unwrap() = Some(format!("{e:#}")),
    }
}

/// 旧版本存的是 Gemini 的模型和声音名，换成现在的默认值。
fn migrate(mut s: Settings) -> Settings {
    let d = Settings::default();
    if !tts::MODELS.contains(&s.model.as_str()) {
        s.model = d.model;
    }
    // Fish 的声音 id 是 32 位十六进制；不是的话是旧版本的 Gemini 声音名
    if !(s.voice.len() == 32 && s.voice.chars().all(|c| c.is_ascii_hexdigit())) {
        s.voice = d.voice;
    }
    s
}

/// 第一次运行：直播输出默认选虚拟声卡（BlackHole / VB-CABLE），找不到就用系统默认输出。
fn first_run_settings() -> Settings {
    let mut s = Settings::default();
    if let Ok(outs) = audio::outputs()
        && let Some(d) = outs.iter().find(|d| d.name.contains("BlackHole") || d.name.contains("CABLE Input"))
    {
        s.output = d.id.clone();
    }
    s
}

/// 开发测试用（只在调试版里有）：VOICE_RELAY_TEST_JS 指向一个脚本。设了它，窗口不显示，
/// 设置存在脚本旁边、不碰用户的设置文件，页面加载完执行这个脚本，
/// 脚本用 "test-report" 事件发出的内容打印到标准输出。
fn test_script() -> Option<(PathBuf, String)> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let path = PathBuf::from(std::env::var_os("VOICE_RELAY_TEST_JS")?);
    let js = std::fs::read_to_string(&path).ok()?;
    Some((path, js))
}

fn main() {
    let test = test_script();
    let test_js = test.as_ref().map(|(_, js)| js.clone());
    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .on_page_load(move |webview, payload| {
            if let (Some(js), PageLoadEvent::Finished) = (&test_js, payload.event()) {
                let _ = webview.eval(js);
            }
        })
        .setup(move |app| {
            let path = match &test {
                Some((script, _)) => script.with_file_name("settings.json"),
                None => app.path().app_config_dir()?.join("settings.json"),
            };
            let settings = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
                .unwrap_or_else(first_run_settings);
            let settings = migrate(settings);
            let key_path = path.with_file_name(KEY_FILE);
            let key = load_key(&key_path, test.is_none());
            app.manage(AppState {
                path,
                settings: Mutex::new(settings),
                pipeline: Mutex::new(None),
                want: AtomicBool::new(false),
                device_lost: AtomicBool::new(false),
                defaults: Mutex::new(Default::default()),
                waiting: Mutex::new(None),
                run: AtomicU64::new(0),
                held: Arc::new(AtomicBool::new(false)),
                key_path,
                key: Mutex::new(key),
                overlay: Mutex::new(None),
                overlay_error: Mutex::new(None),
                hotkey: Mutex::new(None),
                hotkey_error: Mutex::new(None),
            });
            register_hotkey(app.handle());
            let handle = app.handle().clone();
            tauri::async_runtime::block_on(start_overlay(&handle));
            let h = handle.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(DEVICE_CHECK).await;
                    device_check(&h);
                }
            });
            if test.is_some() {
                // 不在程序坞里出现
                #[cfg(target_os = "macos")]
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
                app.listen_any("test-report", |e| println!("[test] {}", e.payload()));
            } else if let Some(w) = app.get_webview_window("main") {
                w.show()?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            save_settings,
            set_api_key,
            clear_api_key,
            start,
            stop,
            say,
            stop_speech,
            preview,
            own_voices
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                stop_pipeline(&window.state::<AppState>());
            }
        })
        .run(tauri::generate_context!())
        .expect("启动失败");
}
