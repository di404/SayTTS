const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const MAX_ROWS = 80;

let snap = null;
/** 账号里自己的声音，启动和保存 key 后读取 */
let ownVoices = [];
let mode = "auto";
// 第几次开始。句子编号每次从 1 起；换设置重启时上一次的事件可能晚到，只认这一次的
let run = 0;
const rows = new Map();
let lostCount = 0;
let lastProblem = "";

if (/Mac/.test(navigator.platform)) document.body.classList.add("mac");
// 区分键盘和鼠标操作，下拉框的焦点环只给键盘用户看
document.addEventListener("keydown", (e) => { if (e.key === "Tab") document.body.classList.add("kbd"); });
document.addEventListener("mousedown", () => document.body.classList.remove("kbd"));

// ---------------------------------------------------------------- 错误

function showError(msg) {
  $("error").textContent = msg || "";
  $("error").hidden = !msg;
}
$("error").addEventListener("click", () => showError(""));

async function call(cmd, args) {
  try {
    return await invoke(cmd, args);
  } catch (e) {
    showError(String(e));
    throw e;
  }
}

// ---------------------------------------------------------------- 设置

function setValue(el, v) {
  if (document.activeElement !== el) el.value = v;
}

function fillDevices(sel, list, value, first) {
  if (document.activeElement === sel) return; // 用户正在选
  sel.replaceChildren(new Option(first, ""));
  for (const d of list) sel.add(new Option(d.name, d.id));
  if (value && !list.some((d) => d.id === value)) sel.add(new Option("已断开的设备", value));
  sel.value = value;
}

function render(s) {
  snap = s;
  const st = s.settings;
  fillDevices($("input"), s.inputs, st.input, "系统默认");
  fillDevices($("output"), s.outputs, st.output, "系统默认");
  fillDevices($("monitor"), s.outputs, st.monitor, "不监听");
  setValue($("monitor_volume"), Math.round(st.monitor_volume * 100));
  $("monitor_volume").hidden = !st.monitor;
  $("echo-box").hidden = !st.monitor;
  $("echo_gate").checked = st.echo_gate;

  mode = st.mode === "ptt" ? "ptt" : "auto";
  for (const b of document.querySelectorAll(".seg button")) b.setAttribute("aria-pressed", String(b.dataset.mode === mode));
  $("gate-box").hidden = mode !== "auto";
  $("max-box").hidden = mode !== "auto";
  $("ptt-box").hidden = mode !== "ptt";
  setValue($("gate"), st.gate_ms);
  setValue($("max_s"), st.max_s);
  setValue($("ptt_key"), st.ptt_key);
  $("ptt_key").classList.toggle("bad", !!s.hotkey_error);

  fillVoices($("voice"), s.voices, (v) => `${v.name} · ${v.desc}`, s.voice.voice);
  fillVoices($("voice-quick"), s.voices, (v) => v.name, s.voice.voice);
  fitSelect($("voice-quick"));

  $("listen").hidden = !st.monitor;
  $("listen").setAttribute("aria-pressed", String(!st.monitor_muted));
  $("listen").title = st.monitor_muted ? "监听已关" : "监听中";
  $("model").replaceChildren(...s.models.map((m) => new Option(m, m)));
  $("model").value = s.voice.model;
  setValue($("vocabulary"), st.vocabulary.join("，"));
  setValue($("proxy"), st.proxy);

  fillProviders("tts", s.providers, st.tts_provider);
  fillProviders("stt", s.providers, st.stt_provider);
  renderKeys(s.keys);
  const missing = missingKeys(s);
  $("setup-text").textContent = `需要 ${missing.map((k) => k.name).join("、")} API key`;
  $("setup").hidden = !missing.length;

  $("overlay").value = s.overlay_url || "";
  $("overlay").classList.toggle("bad", !!s.overlay_error);
  setValue($("port"), st.overlay_port);

  setRunning(s.running || s.want);
  if (s.want && !s.running) setStatus("等待设备", "wait", s.waiting);
  const needsKey = missing.length > 0;
  $("badge").hidden = !(needsKey || s.hotkey_error || s.overlay_error);
  if (needsKey || s.overlay_error) $("advanced").open = true;
  // 同一个问题只弹一次，用户点掉后刷新不再弹出来
  const problem = s.hotkey_error || s.overlay_error || (s.want && !s.running && s.waiting) || "";
  if (problem && problem !== lastProblem) showError(problem);
  lastProblem = problem;
}

function collect() {
  return {
    ...snap.settings,
    input: $("input").value,
    output: $("output").value,
    monitor: $("monitor").value,
    monitor_volume: Number($("monitor_volume").value) / 100,
    echo_gate: $("echo_gate").checked,
    mode,
    ptt_key: $("ptt_key").value.trim(),
    gate_ms: Number($("gate").value) || 500,
    max_s: Number($("max_s").value) || 10,
    tts_provider: $("tts_provider").value || snap.settings.tts_provider,
    stt_provider: $("stt_provider").value || snap.settings.stt_provider,
    // 声音只属于当前朗读平台；换了朗读平台时，这次不改声音，用新平台上次的选择
    voices: $("tts_provider").value && $("tts_provider").value !== snap.settings.tts_provider
      ? snap.settings.voices
      : { ...snap.settings.voices, [snap.settings.tts_provider]: { model: $("model").value, voice: $("voice").value } },
    vocabulary: $("vocabulary").value.split(/[,，、\n]/).map((w) => w.trim()).filter(Boolean),
    proxy: $("proxy").value.trim(),
    overlay_port: Number($("port").value) || 7788,
  };
}

async function save() {
  const was = snap.settings.tts_provider;
  render(await call("save_settings", { settings: collect() }));
  if (snap.settings.tts_provider !== was) {
    ownVoices = []; // 账号里的声音属于原来的平台
    render(snap);
    loadOwnVoices();
  }
}

const missingKeys = (s) => s.keys.filter((k) => !k.source);

/** 只有一个平台能选时不显示选择框 */
function fillProviders(kind, providers, value) {
  const list = providers.filter((p) => p[kind]);
  $(`${kind}-provider-box`).hidden = list.length < 2;
  const sel = $(`${kind}_provider`);
  if (document.activeElement === sel) return;
  sel.replaceChildren(...list.map((p) => new Option(p.name, p.id)));
  sel.value = value;
}

/** 每个正在用的平台一行 API key，放在代理那一行前面 */
function renderKeys(keys) {
  const old = [...document.querySelectorAll(".key-row")];
  if (old.some((r) => r.querySelector("input") === document.activeElement)) return; // 用户正在输入
  old.forEach((r) => r.remove());
  for (const k of keys) {
    const row = document.createElement("div");
    row.className = "field key-row";
    const id = `key-${k.provider}`;
    const label = Object.assign(document.createElement("label"), { htmlFor: id, textContent: keys.length > 1 ? k.name : "API key" });
    const input = Object.assign(document.createElement("input"), { id, type: "password", autocomplete: "off", spellcheck: false });
    input.placeholder = { saved: "已保存", env: `使用环境变量 ${k.key_env}` }[k.source] || `粘贴 ${k.name} API key`;
    const saveBtn = Object.assign(document.createElement("button"), { type: "button", textContent: "保存" });
    const clearBtn = Object.assign(document.createElement("button"), { type: "button", textContent: "清除", hidden: k.source !== "saved" });
    saveBtn.addEventListener("click", async () => {
      const key = input.value.trim();
      if (!key) return;
      await call("set_api_key", { provider: k.provider, key });
      input.value = "";
      showError("");
      input.blur();
      await refresh();
      loadOwnVoices();
    });
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") saveBtn.click();
    });
    clearBtn.addEventListener("click", async () => {
      await call("clear_api_key", { provider: k.provider });
      refresh();
    });
    row.append(label, input, saveBtn, clearBtn);
    $("proxy-field").before(row);
  }
}

async function refresh() {
  render(await call("snapshot"));
}

/** 我的声音在前，预置声音在后。选中的声音还没读到时先放一个占位，免得保存时丢掉选择。 */
function fillVoices(sel, presets, label, current) {
  const group = (name, opts) => {
    const g = document.createElement("optgroup");
    g.label = name;
    g.append(...opts);
    return g;
  };
  const groups = [];
  if (ownVoices.length) groups.push(group("我的声音", ownVoices.map((v) => new Option(v.name, v.id))));
  groups.push(group("预置声音", presets.map((v) => Object.assign(new Option(label(v), v.id), { title: v.desc }))));
  const known = ownVoices.some((v) => v.id === current) || presets.some((v) => v.id === current);
  sel.replaceChildren(...(known ? [] : [new Option("我的声音", current)]), ...groups);
  sel.value = current;
}

// 原生下拉框按最长的选项定宽；顶栏里只按当前选中的名字定宽
const measure = document.createElement("canvas").getContext("2d");
function fitSelect(sel) {
  const opt = sel.selectedOptions[0];
  if (!opt) return;
  measure.font = getComputedStyle(sel).font;
  sel.style.width = `${Math.ceil(measure.measureText(opt.textContent).width) + 2}px`;
}

async function loadOwnVoices() {
  try {
    ownVoices = await invoke("account_voices");
  } catch {
    return; // 没有 key 或网络不通：只显示预置声音
  }
  if (snap) render(snap);
}

for (const id of ["input", "output", "monitor", "monitor_volume", "echo_gate", "voice", "model", "gate", "max_s", "vocabulary", "proxy", "port", "tts_provider", "stt_provider"]) {
  $(id).addEventListener("change", save);
}
$("voice-quick").addEventListener("change", () => {
  fitSelect($("voice-quick"));
  $("voice").value = $("voice-quick").value;
  save();
});

$("listen").addEventListener("click", () => {
  snap.settings.monitor_muted = !snap.settings.monitor_muted;
  save();
});

for (const b of document.querySelectorAll(".seg button")) {
  b.addEventListener("click", () => {
    mode = b.dataset.mode;
    save();
  });
}

// 点一下按键框，再按想用的键（可以带修饰键）
$("ptt_key").addEventListener("keydown", (e) => {
  if (e.key === "Tab") return;
  e.preventDefault();
  if (["Shift", "Control", "Alt", "Meta"].includes(e.key)) return;
  const parts = [];
  if (e.metaKey) parts.push("Cmd");
  if (e.ctrlKey) parts.push("Ctrl");
  if (e.altKey) parts.push("Alt");
  if (e.shiftKey) parts.push("Shift");
  parts.push(e.code);
  $("ptt_key").value = parts.join("+");
  $("ptt_key").blur();
  save();
});

$("copy").addEventListener("click", async () => {
  const el = $("overlay");
  if (!el.value) return;
  el.select();
  try {
    await navigator.clipboard.writeText(el.value);
  } catch {
    document.execCommand("copy");
  }
  const b = $("copy");
  b.textContent = "已复制";
  setTimeout(() => (b.textContent = "复制"), 1200);
});

// ---------------------------------------------------------------- 设置面板

function openSettings(focusKey) {
  $("home").hidden = true;
  $("settings").hidden = false;
  if (focusKey) {
    $("advanced").open = true;
    const k = snap && (missingKeys(snap)[0] || snap.keys[0]);
    if (k) $(`key-${k.provider}`)?.focus();
  } else {
    $("close-settings").focus();
  }
}

function closeSettings() {
  $("settings").hidden = true;
  $("home").hidden = false;
  $("open-settings").focus();
}

$("open-settings").addEventListener("click", () => openSettings(false));
$("setup-go").addEventListener("click", () => openSettings(true));
$("close-settings").addEventListener("click", closeSettings);
document.addEventListener("keydown", (e) => {
  // 按键框在录键时 Esc 也是可录的键
  if (e.key !== "Escape" || $("settings").hidden || document.activeElement === $("ptt_key")) return;
  closeSettings();
});

$("preview").addEventListener("click", async () => {
  const b = $("preview");
  b.disabled = true;
  try {
    await call("preview", { voice: { model: $("model").value, voice: $("voice").value } });
  } finally {
    b.disabled = false;
  }
});

// ---------------------------------------------------------------- 开始 / 停止

function setStatus(text, cls, title) {
  $("status").textContent = text;
  $("status").title = title || "";
  $("dot").className = `dot ${cls || ""}`;
}

function setRunning(on) {
  const was = $("run").classList.contains("on");
  const label = on ? "停止" : "开始";
  $("run-label").textContent = label;
  $("run").setAttribute("aria-label", label);
  $("run").title = label;
  $("run").classList.toggle("on", on);
  $("stage").classList.toggle("on", on);
  if (!on) {
    setStatus("未开始");
    resetMeter();
    $("interim").textContent = "";
    $("ptt").hidden = true;
    updateHush();
  } else if (!was) {
    setStatus("连接中", "wait");
  }
}

$("run").addEventListener("click", async () => {
  const b = $("run");
  b.disabled = true;
  try {
    if (b.classList.contains("on")) {
      await call("stop");
    } else if (snap && missingKeys(snap).length) {
      openSettings(true);
    } else {
      showError("");
      await call("start");
    }
  } finally {
    b.disabled = false;
    refresh();
  }
});

$("hush").addEventListener("click", () => call("stop_speech"));

$("say-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const text = $("say").value.trim();
  if (!text) return;
  await call("say", { text });
  $("say").value = "";
});

// ---------------------------------------------------------------- 事件

// 电平表（参考 HeitingTUI）：一排固定的竖条，整体随当前响度伸缩，安静时缩到最短。
// 每根柱子各自小幅起伏，中间的比两端长；变大立刻跟上，变小平滑回落。
// 送去识别时用强调色，否则用灰色。
const BAR = 3;
const GAP = 3;
const LOUD_RANGE = 24; // 比近期响度低这么多 dB 时柱子最短
const HEADROOM = 2; // 近期响度离最长柱子留的余量，让重音能顶上去
const GAIN_RISE = 0.3; // s，参考响度追上更响声音的时间常数
const GAIN_FALL = 3; // dB/s，参考响度回落速度
const FLOOR_RISE = 2; // dB/s，底噪估计爬升速度
const NOISE_GATE = 6; // 高出底噪不到这么多 dB 的声音不画
const MIN_SPAN = 18; // 参考响度至少高出底噪这么多 dB
const CONTRAST = 1.8;
const RELEASE = 0.08; // s，回落时间常数
const JITTER_EVERY = 0.09; // s
const JITTER_EASE = 0.04; // s

const vis = {
  ref: -30, floor: -60, db: -120, envelope: 0, speaking: false,
  last: 0, lastLevel: 0, nextPick: 0, jitter: [], targets: [], raf: 0,
};

function meter(level, threshold, speaking) {
  const now = performance.now();
  const dt = vis.lastLevel ? Math.min((now - vis.lastLevel) / 1000, 0.2) : 0.06;
  vis.lastLevel = now;
  vis.speaking = speaking;
  const db = 20 * Math.log10(Math.max(level, 1) / 32768);
  vis.floor = db < vis.floor ? db : vis.floor + FLOOR_RISE * dt;
  vis.floor = Math.min(Math.max(vis.floor, -70), -MIN_SPAN);
  if (db > vis.ref) vis.ref += (db - vis.ref) * (1 - Math.exp(-dt / GAIN_RISE));
  else vis.ref -= GAIN_FALL * dt;
  vis.ref = Math.min(Math.max(vis.ref, vis.floor + MIN_SPAN, -50), 0);
  vis.db = db;
  startMeter();
}

function resetMeter() {
  cancelAnimationFrame(vis.raf);
  Object.assign(vis, { ref: -30, floor: -60, db: -120, envelope: 0, speaking: false, last: 0, lastLevel: 0, raf: 0 });
}

function startMeter() {
  if (vis.raf) return;
  vis.last = performance.now();
  vis.raf = requestAnimationFrame(stepMeter);
}

function stepMeter(now) {
  vis.raf = 0;
  const dt = Math.min((now - vis.last) / 1000, 0.2);
  vis.last = now;

  const top = vis.ref + HEADROOM;
  const bottom = Math.max(top - LOUD_RANGE, vis.floor + NOISE_GATE);
  const target = Math.pow(Math.min(Math.max((vis.db - bottom) / (top - bottom), 0), 1), CONTRAST);
  // 电平事件断了（比如停止或卡顿）就当静音
  const fresh = now - vis.lastLevel < 300;
  const env = fresh ? target : 0;
  vis.envelope = env >= vis.envelope ? env : env + (vis.envelope - env) * Math.exp(-dt / RELEASE);
  if (vis.envelope < 0.005) vis.envelope = 0;

  if ((vis.nextPick -= dt) <= 0) {
    for (let i = 0; i < vis.targets.length; i++) vis.targets[i] = Math.random();
    vis.nextPick = JITTER_EVERY;
  }
  const ease = 1 - Math.exp(-dt / JITTER_EASE);
  for (let i = 0; i < vis.jitter.length; i++) vis.jitter[i] += (vis.targets[i] - vis.jitter[i]) * ease;

  drawMeter();
  if (vis.envelope > 0 || fresh) vis.raf = requestAnimationFrame(stepMeter);
}

function drawMeter() {
  const c = $("meter");
  const dpr = window.devicePixelRatio || 1;
  const w = c.clientWidth;
  const h = c.clientHeight;
  if (c.width !== Math.round(w * dpr) || c.height !== Math.round(h * dpr)) {
    c.width = Math.round(w * dpr);
    c.height = Math.round(h * dpr);
  }
  const g = c.getContext("2d");
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  g.clearRect(0, 0, w, h);

  // 柱子从按钮两侧向外排开，靠近按钮的长，越往外越短
  const btn = $("run");
  const cx = w / 2;
  const inner = btn.offsetWidth / 2 + 14;
  const m = Math.max(1, Math.floor((cx - inner + GAP) / (BAR + GAP)));
  if (vis.jitter.length !== m * 2) {
    vis.jitter = Array.from({ length: m * 2 }, Math.random);
    vis.targets = Array.from({ length: m * 2 }, Math.random);
  }
  const mid = h / 2;
  const css = getComputedStyle(document.documentElement);
  g.fillStyle = css.getPropertyValue(vis.speaking ? "--accent" : "--faint");
  g.globalAlpha = vis.speaking ? 1 : 0.8;
  g.beginPath();
  for (let side = 0; side < 2; side++) {
    for (let k = 0; k < m; k++) {
      const u = m > 1 ? k / (m - 1) : 0; // 贴着按钮 0，最外 1
      const shape = 0.15 + 0.85 * Math.pow(Math.cos((u * Math.PI) / 2), 1.5);
      const v = vis.envelope * shape * (0.35 + 0.65 * vis.jitter[side * m + k]);
      const bh = BAR + v * (h - BAR);
      const off = inner + k * (BAR + GAP);
      const x = side ? cx + off : cx - off - BAR;
      g.roundRect(x, mid - bh / 2, BAR, bh, BAR / 2);
    }
  }
  g.fill();
  g.globalAlpha = 1;
  btn.style.setProperty("--lvl", vis.envelope.toFixed(3));
}

window.addEventListener("resize", drawMeter);
requestAnimationFrame(drawMeter); // 没开始时也画出一排最短的柱子

function row(id, text) {
  const k = `${run}:${id}`;
  return rows.get(k) || newRow(k, text);
}

function newRow(k, text) {
  const li = document.createElement("li");
  const t = document.createElement("span");
  t.className = "text";
  t.textContent = text;
  const tag = document.createElement("span");
  tag.className = "tag";
  li.append(t, tag);
  follow(() => $("log").append(li));
  rows.set(k, li);
  while (rows.size > MAX_ROWS) {
    const [old, el] = rows.entries().next().value;
    el.remove();
    rows.delete(old);
  }
  updateHush();
  return li;
}

function mark(li, cls, tag) {
  li.className = cls;
  li.querySelector(".tag").textContent = tag;
  updateHush();
}

const isActive = (li) => li.className === "busy" || li.className === "play";

// 有句子在合成或播放时才需要「停止朗读」；有读完的句子才需要「清空」
function updateHush() {
  let active = false;
  let finished = false;
  for (const li of rows.values()) {
    if (isActive(li)) active = true;
    else finished = true;
  }
  $("hush").hidden = !active;
  $("clear").hidden = !finished;
  updateEmpty();
}

// 还没有任何句子时录音按钮居中；第一句出来后沉到底部，把空间让给列表
// 过渡动画的回调是异步执行的，所以按目标状态判断，不看当前 class
let emptyTarget = true;
function updateEmpty() {
  const empty = !rows.size && !$("interim").textContent;
  if (empty === emptyTarget) return;
  emptyTarget = empty;
  const home = $("home");
  const apply = () => home.classList.toggle("empty", emptyTarget);
  if (document.startViewTransition && !home.hidden) document.startViewTransition(apply).finished.catch(() => {});
  else apply();
}

// 清空读完、停止、失败的句子；还在合成或播放的留着
$("clear").addEventListener("click", () => {
  for (const [k, li] of rows) {
    if (isActive(li)) continue;
    li.remove();
    rows.delete(k);
  }
  updateHush();
});

// 用户停在底部时，新内容进来就跟着滚；往上翻看时不打扰
function follow(change) {
  const el = $("scroller");
  const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
  change();
  if (atBottom) el.scrollTop = el.scrollHeight;
}

function onEvent(ev) {
  if (ev.type === "started") {
    if (ev.run <= run) return;
    run = ev.run;
    // 上一次没读完的句子不会再有结果
    for (const li of rows.values()) if (isActive(li)) mark(li, "stop", "已停止");
    setRunning(true);
    setStatus("连接中", "wait");
    if (lastProblem) { showError(""); lastProblem = ""; } // 设备恢复了
    return;
  }
  if (ev.run !== run) return;
  switch (ev.type) {
    case "stt_connected":
      setStatus("已连接", "ok");
      break;
    case "stt_disconnected":
      setStatus("重连中", "bad", ev.reason);
      break;
    case "interim":
      follow(() => ($("interim").textContent = ev.text));
      updateEmpty();
      break;
    case "final":
      $("interim").textContent = "";
      break;
    case "added":
      mark(row(ev.id, ev.text), "busy", "合成中");
      break;
    case "playing":
      mark(row(ev.id, ev.text), "play", "播放中");
      break;
    case "done": {
      const li = rows.get(`${run}:${ev.id}`);
      if (li && li.className !== "fail") mark(li, "done", "");
      break;
    }
    case "failed": {
      const li = rows.get(`${run}:${ev.id}`);
      if (!li) break;
      mark(li, "fail", "失败");
      const text = li.querySelector(".text").textContent;
      const why = document.createElement("span");
      why.className = "why";
      why.textContent = ev.error;
      li.querySelector(".text").append(why);
      const retry = document.createElement("button");
      retry.className = "small retry";
      retry.type = "button";
      retry.textContent = "重试";
      retry.addEventListener("click", () => call("say", { text }).then(() => retry.remove()));
      li.insertBefore(retry, li.querySelector(".tag"));
      break;
    }
    case "cleared":
      for (const li of rows.values()) if (isActive(li)) mark(li, "stop", "已停止");
      break;
    case "level":
      meter(ev.level, ev.threshold, ev.speaking);
      break;
    case "lost":
      mark(newRow(`lost:${++lostCount}`, `一句话没转写出来（${(ev.ms / 1000).toFixed(1)} 秒），转写服务一直连不上`), "lost", "已丢失");
      break;
    case "device_lost":
      setStatus("设备断开，正在重新打开", "wait", ev.message);
      break;
    case "error":
      showError(ev.message);
      break;
  }
}

listen("pipeline", (e) => onEvent(e.payload));
// 设备断开后后台自动重开，状态变了就刷新
listen("state", refresh);
listen("ptt", (e) => {
  $("ptt").hidden = !e.payload;
});

// 插拔设备后回到窗口时刷新列表
window.addEventListener("focus", refresh);
refresh().then(loadOwnVoices);
