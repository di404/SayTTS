# /// script
# requires-python = ">=3.11"
# dependencies = ["httpx[http2]>=0.27", "websockets>=15"]
# ///
"""Gemini 云端语音链路实测：gemini-3.5-transcribe-live（转写）+ gemini-3.8 TTS（流式合成）。

  tts  两个 TTS 模型的首包延迟和合成速度，样本存到 spike/out/ 方便试听
  stt  说完话到拿到定稿文字要多久（hybrid = 本地静音 300ms 后发 audioStreamEnd；server = 等服务端 VAD）
  e2e  说完话 → 定稿文字 → TTS 第一块音频

用法：uv run spike/latency_bench.py [tts|stt|all] [--runs 2] [--debug]
需要 GEMINI_API_KEY；HTTP 和 WebSocket 都走 HTTPS_PROXY。
"""
import argparse
import asyncio
import base64
import difflib
import json
import os
import re
import statistics
import subprocess
import time
import wave
from array import array
from pathlib import Path

import httpx
import websockets

API = "https://generativelanguage.googleapis.com/v1beta"
WS_URL = ("wss://generativelanguage.googleapis.com/ws/"
          "google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent")
STT_MODEL = "models/gemini-3.5-transcribe-live"
TTS_MODELS = ["gemini-3.8-flash-tts", "gemini-3.8-flash-lite-tts"]
VOICE = "Kore"
OUT = Path(__file__).resolve().parent / "out"

TTS_TEXTS = {
    "short": "大家晚上好！",
    "medium": "今天我们来玩一下这个新出的解谜游戏，看看能不能一次通关。",
    "long": "刚才那个机关我其实早就看出来了，只是想多试几次，给大家展示一下不同的解法，你们别在弹幕里笑我啊。",
}
STT_TEXTS = [
    "今天我们来玩一下这个新出的解谜游戏，看看能不能一次通关。",
    "这个 boss 的二阶段有点难，我先去 check 一下攻略。",
]


def api_key() -> str:
    k = os.environ.get("GEMINI_API_KEY")
    if not k:
        raise SystemExit("需要环境变量 GEMINI_API_KEY")
    return k


def save_wav(path: Path, pcm: bytes, rate: int) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(pcm)


def resample(pcm: bytes, src: int, dst: int) -> bytes:
    p = subprocess.run(
        ["ffmpeg", "-loglevel", "error", "-f", "s16le", "-ar", str(src), "-ac", "1", "-i", "pipe:0",
         "-f", "s16le", "-ar", str(dst), "-ac", "1", "pipe:1"],
        input=pcm, capture_output=True, check=True)
    return p.stdout


def voiced_span(pcm: bytes, rate: int, thresh: int = 600) -> tuple[float, float]:
    s = array("h")
    s.frombytes(pcm)
    idx = [i for i, v in enumerate(s) if abs(v) > thresh]
    if not idx:
        return 0.0, len(s) / rate
    return idx[0] / rate, idx[-1] / rate


def walk_audio(obj):
    if isinstance(obj, dict):
        if obj.get("type") == "audio" and isinstance(obj.get("data"), str):
            yield obj["data"]
        for v in obj.values():
            yield from walk_audio(v)
    elif isinstance(obj, list):
        for v in obj:
            yield from walk_audio(v)


def redact(obj):
    if isinstance(obj, dict):
        return {k: (f"<{len(v)} chars>" if k == "data" and isinstance(v, str) else redact(v))
                for k, v in obj.items()}
    if isinstance(obj, list):
        return [redact(v) for v in obj]
    return obj


def norm(t: str) -> str:
    return re.sub(r"[\s\W_]+", "", t).lower()


async def tts(client: httpx.AsyncClient, model: str, text: str, *, rate: int = 24000,
              voice: str = VOICE, style: str | None = None, debug: bool = False) -> dict:
    part: dict = {"type": "text", "text": text}
    if style:
        part["annotations"] = [{"type": "speech_metadata", "style": style}]
    body = {
        "model": model,
        "input": [{"type": "user_input", "content": [part]}],
        "response_format": {"type": "audio", "mime_type": "audio/l16", "sample_rate": rate},
        "generation_config": {"speech_config": [{"voice": voice}]},
        "stream": True,
        "store": False,  # 默认 store=true 会在服务端存档，实测首包多出约 3 秒
    }
    t0 = time.perf_counter()
    first = None
    pcm = bytearray()
    kinds: dict[str, int] = {}
    shown: list[str] = []
    other_events: list = []
    lines_all: list[str] = []
    async with client.stream("POST", f"{API}/interactions", json=body,
                             headers={"x-goog-api-key": api_key()}, timeout=60) as r:
        t_head = time.perf_counter() - t0
        if r.status_code != 200:
            err = (await r.aread()).decode(errors="replace")
            raise RuntimeError(f"{model} HTTP {r.status_code}: {err[:800]}")

        def handle(name, payload):
            nonlocal first
            if payload.strip() in ("", "[DONE]"):
                return
            ev = json.loads(payload)
            kind = ev.get("event_type") or name or "?"
            kinds[kind] = kinds.get(kind, 0) + 1
            if debug and len(shown) < 8:
                shown.append(json.dumps(redact(ev), ensure_ascii=False)[:500])
            d = ev.get("delta") or {}
            if kind == "step.delta" and d.get("type") == "audio" and d.get("data"):
                if first is None:
                    first = time.perf_counter() - t0
                pcm.extend(base64.b64decode(d["data"]))
            elif kind != "step.delta":
                other_events.append(ev)

        ev_name, data = None, []
        async for line in r.aiter_lines():
            lines_all.append(line)
            if line.startswith("event:"):
                ev_name = line[6:].strip()
            elif line.startswith("data:"):
                data.append(line[5:].lstrip())
            elif not line.strip():
                if data:
                    handle(ev_name, "\n".join(data))
                ev_name, data = None, []
        if data:
            handle(ev_name, "\n".join(data))
    total = time.perf_counter() - t0

    note = ""
    if not pcm:  # 没有增量音频：退回到在完整响应里找音频
        sources = other_events or [json.loads("\n".join(lines_all))] if not kinds else other_events
        for src in sources:
            for b64 in walk_audio(src):
                pcm.extend(base64.b64decode(b64))
        first = total
        note = "（未见增量音频块，整段一次返回）"
    audio_s = len(pcm) / 2 / rate
    return {"model": model, "chars": len(text), "head": t_head, "first": first, "total": total,
            "audio_s": audio_s, "kinds": kinds, "shown": shown, "pcm": bytes(pcm), "rate": rate,
            "note": note}


async def stt(pcm16k: bytes, expected: str, *, mode: str | None = None,
              finalize: str = "hybrid", tail_ms: int = 300, debug: bool = False) -> dict:
    cfg: dict = {"languageCodes": []}
    if mode:
        cfg["mode"] = mode
    setup = {"setup": {"model": STT_MODEL,
                       "generationConfig": {"responseModalities": ["TEXT"]},
                       "inputAudioTranscription": cfg}}
    start_s, end_s = voiced_span(pcm16k, 16000)
    speech = pcm16k[: int((end_s + tail_ms / 1000) * 16000) * 2]
    log: list[tuple[float, str, str]] = []
    others: list[str] = []
    final_evt = asyncio.Event()

    t0 = time.perf_counter()
    async with websockets.connect(f"{WS_URL}?key={api_key()}", max_size=None,
                                  open_timeout=20) as ws:
        t_open = time.perf_counter() - t0
        await ws.send(json.dumps(setup))
        first_msg = json.loads(await asyncio.wait_for(ws.recv(), 20))
        t_setup = time.perf_counter() - t0
        if "setupComplete" not in first_msg:
            raise RuntimeError(f"setup 失败: {json.dumps(first_msg, ensure_ascii=False)[:500]}")

        async def rx():
            async for raw in ws:
                m = json.loads(raw)
                now = time.perf_counter()
                sc = m.get("serverContent") or {}
                hit = False
                if "interimInputTranscription" in sc:
                    log.append((now, "interim", sc["interimInputTranscription"].get("text", "")))
                    hit = True
                if "inputTranscription" in sc:
                    log.append((now, "final", sc["inputTranscription"].get("text", "")))
                    final_evt.set()
                    hit = True
                if not hit and len(others) < 10:
                    others.append(json.dumps(m, ensure_ascii=False)[:300])

        rx_task = asyncio.create_task(rx())
        chunk = 3200  # 100 ms @ 16 kHz 16-bit mono

        async def send(b: bytes):
            await ws.send(json.dumps({"realtimeInput": {"audio": {
                "data": base64.b64encode(b).decode(), "mimeType": "audio/pcm;rate=16000"}}}))

        t_start = time.perf_counter()
        n = 0
        for i in range(0, len(speech), chunk):
            delay = t_start + n * 0.1 - time.perf_counter()
            if delay > 0:
                await asyncio.sleep(delay)
            await send(speech[i:i + chunk])
            n += 1
        t_speech_start = t_start + start_s
        t_speech_end = t_start + end_s
        if finalize == "hybrid":
            delay = t_start + len(speech) / 32000 - time.perf_counter()
            if delay > 0:
                await asyncio.sleep(delay)
            await ws.send(json.dumps({"realtimeInput": {"audioStreamEnd": True}}))
        else:
            silence = bytes(chunk)
            k = 0
            while not final_evt.is_set() and k < 50:
                delay = t_start + (n + k) * 0.1 - time.perf_counter()
                if delay > 0:
                    await asyncio.sleep(delay)
                await send(silence)
                k += 1
        try:
            await asyncio.wait_for(final_evt.wait(), 10)
        except asyncio.TimeoutError:
            pass
        await asyncio.sleep(1.0)  # 收完可能分段到达的定稿
        rx_task.cancel()

    finals = [(t, x) for t, k, x in log if k == "final"]
    interims = [(t, x) for t, k, x in log if k == "interim"]
    if mode == "SMART":
        text = finals[-1][1] if finals else ""
    else:
        text = "".join(x for _, x in finals)
    sim = difflib.SequenceMatcher(None, norm(expected), norm(text)).ratio()
    after_end = [t for t, _ in finals if t >= t_speech_end - 0.05]
    return {
        "mode": mode or "VERBATIM(默认)", "finalize": finalize,
        "open": t_open, "setup": t_setup,
        "first_interim": (interims[0][0] - t_speech_start) if interims else None,
        "n_interim": len(interims), "n_final": len(finals),
        "final_lat": (after_end[-1] - t_speech_end) if after_end else None,
        "first_final_lat": (after_end[0] - t_speech_end) if after_end else None,
        "text": text, "expected": expected, "sim": sim,
        "finals": [x for _, x in finals],
        "interims": [x for _, x in interims][:6], "others": others,
    }


def ms(x):
    return "—" if x is None else f"{x * 1000:.0f}ms"


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("what", nargs="?", default="all", choices=["tts", "stt", "all"])
    ap.add_argument("--runs", type=int, default=2)
    ap.add_argument("--debug", action="store_true")
    args = ap.parse_args()

    async with httpx.AsyncClient(http2=True, timeout=60) as client:
        t0 = time.perf_counter()
        r = await client.get(f"{API}/models", params={"pageSize": 1},
                             headers={"x-goog-api-key": api_key()})
        print(f"预热连接（TLS+代理）: {ms(time.perf_counter() - t0)}  HTTP {r.status_code}  {r.http_version}")

        ttfa_by_model: dict[str, float | None] = {}
        if args.what in ("tts", "all"):
            print("\n== TTS 流式合成（Interactions API, audio/l16 24kHz, voice=%s）" % VOICE)
            for model in TTS_MODELS:
                firsts = []
                for name, text in TTS_TEXTS.items():
                    for run in range(args.runs):
                        try:
                            res = await tts(client, model, text, debug=args.debug and run == 0 and name == "short")
                        except Exception as e:  # noqa: BLE001
                            print(f"  {model:28s} {name:6s} 失败: {e}")
                            break
                        firsts.append(res["first"])
                        rtf = res["total"] / res["audio_s"] if res["audio_s"] else float("nan")
                        print(f"  {model:28s} {name:6s} {res['chars']:3d}字  首包 {ms(res['first']):>7s}"
                              f"  全部 {ms(res['total']):>7s}  音频 {res['audio_s']:.1f}s  RTF {rtf:.2f} {res['note']}")
                        if res["shown"]:
                            print("    事件类型:", res["kinds"])
                            for s in res["shown"]:
                                print("    ", s)
                        if run == 0:
                            save_wav(OUT / f"{model}-{name}.wav", res["pcm"], res["rate"])
                ttfa_by_model[model] = med(firsts)
                print(f"  → {model} 首包中位数 {ms(ttfa_by_model[model])}")

        if args.what in ("stt", "all"):
            print("\n== 转写（gemini-3.5-transcribe-live, 16kHz PCM, 按实时节奏推流）")
            src = []
            for i, text in enumerate(STT_TEXTS):
                # 实测 l16 流式只接受 24 kHz（16000 返回 400），这里合成后再降采样
                res = await tts(client, TTS_MODELS[0], text, voice="Puck")
                pcm16k = resample(res["pcm"], 24000, 16000)
                save_wav(OUT / f"stt-source-{i}.wav", pcm16k, 16000)
                src.append((text, pcm16k))
            cases = [(0, None, "hybrid"), (1, None, "hybrid"), (0, None, "server"), (1, "SMART", "hybrid")]
            stt_lat = []
            for idx, mode, fin in cases:
                text, pcm = src[idx]
                try:
                    s = await stt(pcm, text, mode=mode, finalize=fin, debug=args.debug)
                except Exception as e:  # noqa: BLE001
                    print(f"  句{idx} {mode} {fin} 失败: {e}")
                    continue
                print(f"  句{idx} {s['mode']:12s} {fin:6s} 建连 {ms(s['open'])} setup {ms(s['setup'])}"
                      f" | 首个临时结果 {ms(s['first_interim'])}（距开口） | 定稿 {ms(s['final_lat'])}（距说完）"
                      f" | 临时×{s['n_interim']} 定稿×{s['n_final']} | 相似度 {s['sim']:.2f}")
                print(f"     原文: {s['expected']}")
                print(f"     转写: {s['text']}")
                if args.debug:
                    print("     定稿分段:", s["finals"])
                    print("     临时结果:", s["interims"])
                    if s["others"]:
                        print("     其它消息:", s["others"][:4])
                if fin == "hybrid" and mode is None:
                    stt_lat.append(s["final_lat"])

                if fin == "hybrid" and mode is None and s["text"]:
                    for model in TTS_MODELS:
                        try:
                            t = await tts(client, model, s["text"])
                        except Exception as e:  # noqa: BLE001
                            print(f"     e2e {model} 失败: {e}")
                            continue
                        e2e = (s["final_lat"] or 0) + (t["first"] or 0)
                        print(f"     e2e 说完→听到第一声 ≈ {ms(e2e)}  （定稿 {ms(s['final_lat'])} + {model} 首包 {ms(t['first'])}）")
            print(f"  → hybrid 定稿延迟中位数 {ms(med(stt_lat))}")


if __name__ == "__main__":
    asyncio.run(main())
