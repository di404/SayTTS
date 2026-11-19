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

  fillVoices($("voice"), s.voices, (v) => `${v.name} · ${v.trait_zh}`, st.voice);
  fillVoices($("voice-quick"), s.voices, (v) => v.name, st.voice);

  $("listen").hidden = !st.monitor;
  $("listen").setAttribute("aria-pressed", String(!st.monitor_muted));
  $("listen").title = st.monitor_muted ? "监听已关" : "监听中";
  $("model").replaceChildren(...s.models.map((m) => new Option(m, m)));
  $("model").value = st.model;
  setValue($("vocabulary"), st.vocabulary.join("，"));
  setValue($("proxy"), st.proxy);

  $("key").placeholder = { saved: "已保存", env: "使用环境变量 FISH_API_KEY" }[s.key_source] || "粘贴 Fish Audio API key";
  $("key-clear").hidden = s.key_source !== "saved";
  $("setup").hidden = !!s.key_source;

  $("overlay").value = s.overlay_url || "";
  $("overlay").classList.toggle("bad", !!s.overlay_error);
  setValue($("port"), st.overlay_port);

  setRunning(s.running);
  const needsKey = !s.key_source;
  $("badge").hidden = !(needsKey || s.hotkey_error || s.overlay_error);
  if (needsKey || s.overlay_error) $("advanced").open = true;
  const problem = s.hotkey_error || s.overlay_error;
  if (problem) showError(problem);
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
    model: $("model").value,
    voice: $("voice").value,
    vocabulary: $("vocabulary").value.split(/[,，、\n]/).map((w) => w.trim()).filter(Boolean),
    proxy: $("proxy").value.trim(),
    overlay_port: Number($("port").value) || 7788,
  };
}

async function save() {
  render(await call("save_settings", { settings: collect() }));
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
  groups.push(group("预置声音", presets.map((v) => Object.assign(new Option(label(v), v.id), { title: v.trait_zh }))));
  const known = ownVoices.some((v) => v.id === current) || presets.some((v) => v.id === current);
  sel.replaceChildren(...(known ? [] : [new Option("我的声音", current)]), ...groups);
  sel.value = current;
}

async function loadOwnVoices() {
  try {
    ownVoices = await invoke("own_voices");
  } catch {
    return; // 没有 key 或网络不通：只显示预置声音
  }
  if (snap) render(snap);
}

for (const id of ["input", "output", "monitor", "monitor_volume", "echo_gate", "voice", "model", "gate", "max_s", "vocabulary", "proxy", "port"]) {
  $(id).addEventListener("change", save);
}
$("voice-quick").addEventListener("change", () => {
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

$("key-save").addEventListener("click", async () => {
  const key = $("key").value.trim();
  if (!key) return;
  await call("set_api_key", { key });
  $("key").value = "";
  showError("");
  await refresh();
  loadOwnVoices();
});
$("key").addEventListener("keydown", (e) => {
  if (e.key === "Enter") $("key-save").click();
});
$("key-clear").addEventListener("click", async () => {
  await call("clear_api_key");
  refresh();
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
    $("key").focus();
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
  $("run").textContent = on ? "停止" : "开始";
  $("run").classList.toggle("on", on);
  $("meter").hidden = !on;
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
    } else if (snap && !snap.key_source) {
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
  if ($("meter").hidden) return;
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
  if (c.hidden) return;
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

  const n = Math.max(1, Math.floor((w + GAP) / (BAR + GAP)));
  if (vis.jitter.length !== n) {
    vis.jitter = Array.from({ length: n }, Math.random);
    vis.targets = Array.from({ length: n }, Math.random);
  }
  const x0 = (w - (n * BAR + (n - 1) * GAP)) / 2;
  const mid = h / 2;
  const half = (n - 1) / 2;
  const css = getComputedStyle(document.documentElement);
  g.fillStyle = css.getPropertyValue(vis.speaking ? "--accent" : "--muted");
  g.globalAlpha = vis.speaking ? 1 : 0.55;
  g.beginPath();
  for (let i = 0; i < n; i++) {
    const u = half > 0 ? Math.abs(i - half) / half : 0; // 中间 0，两端 1
    const shape = 0.6 + 0.4 * Math.cos((u * Math.PI) / 2);
    const v = vis.envelope * shape * (0.35 + 0.65 * vis.jitter[i]);
    const bh = BAR + v * (h - BAR);
    g.roundRect(x0 + i * (BAR + GAP), mid - bh / 2, BAR, bh, BAR / 2);
  }
  g.fill();
  g.globalAlpha = 1;
}

window.addEventListener("resize", drawMeter);

function row(id, text) {
  const k = `${run}:${id}`;
  let li = rows.get(k);
  if (!li) {
    li = document.createElement("li");
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
  }
  return li;
}

function mark(li, cls, tag) {
  li.className = cls;
  li.querySelector(".tag").textContent = tag;
  updateHush();
}

// 有句子在合成或播放时才需要「停止朗读」
function updateHush() {
  let active = false;
  for (const li of rows.values()) if (li.className === "busy" || li.className === "play") active = true;
  $("hush").hidden = !active;
}

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
    for (const li of rows.values()) if (li.className === "busy" || li.className === "play") mark(li, "stop", "已停止");
    setRunning(true);
    setStatus("连接中", "wait");
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
      if (li && li.className !== "fail") mark(li, "done", "已读");
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
      for (const li of rows.values()) if (li.className === "busy" || li.className === "play") mark(li, "stop", "已停止");
      break;
    case "level":
      meter(ev.level, ev.threshold, ev.speaking);
      break;
    case "error":
      showError(ev.message);
      break;
  }
}

listen("pipeline", (e) => onEvent(e.payload));
listen("ptt", (e) => {
  $("ptt").hidden = !e.payload;
});

// 插拔设备后回到窗口时刷新列表
window.addEventListener("focus", refresh);
refresh().then(loadOwnVoices);
