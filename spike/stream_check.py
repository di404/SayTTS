# /// script
# requires-python = ">=3.11"
# dependencies = ["httpx[http2]>=0.27", "websockets>=15"]
# ///
"""长会话实测：一个转写会话里连续说多句（中间有停顿和长时间空闲），本地静音门限断句。

  默认   用合成语音拼一段约 60 秒的时间线；同时测 TTS 超短句和空闲 60 秒后的首包
  --wav  用你自己的录音（任意格式，ffmpeg 转 16 kHz 单声道），检验真实麦克风和环境噪声
  --gate 静音多少毫秒算说完（默认 500）

用法：uv run spike/stream_check.py [--wav 录音.m4a] [--gate 500]
"""
import argparse
import asyncio
import base64
import json
import math
import subprocess
import time
from array import array
from collections import deque

import httpx
import websockets

from latency_bench import (API, OUT, STT_MODEL, WS_URL, api_key, ms, resample, save_wav,
                           tts, voiced_span)

RATE = 16000
FRAME = 640          # 20 ms @ 16 kHz, 16-bit
SEND_BYTES = 3200    # 每 100 ms 发一次


def rms(frame: bytes) -> float:
    s = array("h")
    s.frombytes(frame)
    return math.sqrt(sum(v * v for v in s) / max(1, len(s)))


def silence(sec: float) -> bytes:
    return bytes(int(sec * RATE) * 2)


def trim(pcm: bytes) -> bytes:
    a, b = voiced_span(pcm, RATE)
    return pcm[int(max(0, a - 0.05) * RATE) * 2: int((b + 0.05) * RATE) * 2]


def threshold_for(pcm: bytes) -> float:
    levels = sorted(rms(pcm[i:i + FRAME]) for i in range(0, len(pcm) - FRAME, FRAME))
    floor = levels[len(levels) // 5] if levels else 0
    return max(300.0, floor * 3)


async def run_session(pcm: bytes, gate_ms: int, label: str, preroll_ms: int = 300) -> list[dict]:
    """按实时节奏推流。本地能量门限：静音满 gate_ms 就发 audioStreamEnd，并停止发送，直到再次开口。"""
    thr = threshold_for(pcm)
    setup = {"setup": {"model": STT_MODEL,
                       "generationConfig": {"responseModalities": ["TEXT"]},
                       "inputAudioTranscription": {"languageCodes": []}}}
    finals: list[tuple[float, str]] = []
    events: list[str] = []
    closed: list[str] = []
    segs: list[dict] = []

    async with websockets.connect(f"{WS_URL}?key={api_key()}", max_size=None, open_timeout=20) as ws:
        await ws.send(json.dumps(setup))
        first = json.loads(await asyncio.wait_for(ws.recv(), 20))
        if "setupComplete" not in first:
            raise RuntimeError(json.dumps(first, ensure_ascii=False)[:400])

        async def rx():
            try:
                async for raw in ws:
                    m = json.loads(raw)
                    now = time.perf_counter()
                    sc = m.get("serverContent") or {}
                    if "inputTranscription" in sc:
                        finals.append((now, sc["inputTranscription"].get("text", "")))
                    if "goAway" in m or "error" in m:
                        events.append(json.dumps(m, ensure_ascii=False)[:200])
            except websockets.ConnectionClosed as e:
                closed.append(f"code={e.code} reason={e.reason!r}")

        rx_task = asyncio.create_task(rx())

        async def send_audio(b: bytes):
            await ws.send(json.dumps({"realtimeInput": {"audio": {
                "data": base64.b64encode(b).decode(), "mimeType": f"audio/pcm;rate={RATE}"}}}))

        t0 = time.perf_counter()
        speaking, quiet = False, 0
        batch = bytearray()
        pre = deque(maxlen=preroll_ms // 20)
        for k in range(0, len(pcm) // FRAME):
            f = pcm[k * FRAME:(k + 1) * FRAME]
            frame_end = t0 + (k + 1) * 0.02
            d = frame_end - time.perf_counter()
            if d > 0:
                await asyncio.sleep(d)
            if closed:
                break
            voiced = rms(f) > thr
            if not speaking:
                pre.append(f)
                if voiced:
                    speaking, quiet = True, 0
                    batch.extend(b"".join(pre))
                    pre.clear()
                    segs.append({"start": frame_end, "last_voiced": frame_end})
                continue
            batch.extend(f)
            if voiced:
                quiet = 0
                segs[-1]["last_voiced"] = frame_end
            else:
                quiet += 20
            if len(batch) >= SEND_BYTES:
                await send_audio(bytes(batch))
                batch.clear()
            if quiet >= gate_ms:
                if batch:
                    await send_audio(bytes(batch))
                    batch.clear()
                await ws.send(json.dumps({"realtimeInput": {"audioStreamEnd": True}}))
                segs[-1]["end_sent"] = time.perf_counter()
                speaking = False
        if speaking and not closed:
            if batch:
                await send_audio(bytes(batch))
            await ws.send(json.dumps({"realtimeInput": {"audioStreamEnd": True}}))
            segs[-1]["end_sent"] = time.perf_counter()
        await asyncio.sleep(4)
        rx_task.cancel()

    # 每段之后、下一段开口之前到达的定稿，算这一段的
    for i, s in enumerate(segs):
        nxt = segs[i + 1]["start"] if i + 1 < len(segs) else math.inf
        mine = [(t, x) for t, x in finals if s["start"] <= t < nxt]
        s["texts"] = [x for _, x in mine]
        s["lat"] = (mine[0][0] - s["last_voiced"]) if mine else None
        s["at"] = s["start"] - t0
    print(f"\n[{label}] 门限 {gate_ms}ms  阈值 {thr:.0f}  切出 {len(segs)} 段  定稿 {len(finals)} 条"
          + (f"  连接关闭: {closed[0]}" if closed else "  连接全程未断"))
    for i, s in enumerate(segs):
        print(f"  段{i} @{s['at']:5.1f}s  说完→定稿 {ms(s['lat']):>7s}  {' | '.join(s['texts']) or '（没有定稿）'}")
    for e in events:
        print("  事件:", e)
    return segs


async def tts_checks():
    async with httpx.AsyncClient(http2=True, timeout=60) as c:
        await c.get(f"{API}/models", params={"pageSize": 1}, headers={"x-goog-api-key": api_key()})
        out = ["\n[TTS 超短句 + 空闲后首包] gemini-3.8-flash-lite-tts"]
        for text in ["嗯。", "好的", "哈哈", "啊？"]:
            try:
                r = await tts(c, "gemini-3.8-flash-lite-tts", text)
                out.append(f"  {text:4s} 首包 {ms(r['first']):>7s}  音频 {r['audio_s']:.2f}s")
                save_wav(OUT / f"short-{text}.wav", r["pcm"], r["rate"])
            except Exception as e:  # noqa: BLE001
                out.append(f"  {text:4s} 失败: {str(e)[:120]}")
        r = await tts(c, "gemini-3.8-flash-lite-tts", "连接热的时候。")
        out.append(f"  热连接       首包 {ms(r['first']):>7s}")
        await asyncio.sleep(60)
        r = await tts(c, "gemini-3.8-flash-lite-tts", "空闲一分钟以后。")
        out.append(f"  空闲 60s 后  首包 {ms(r['first']):>7s}")
        return out


async def synth(c, text):
    r = await tts(c, "gemini-3.8-flash-tts", text, voice="Puck")
    return trim(resample(r["pcm"], 24000, RATE))


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--wav")
    ap.add_argument("--gate", type=int, default=500)
    args = ap.parse_args()

    if args.wav:
        pcm = subprocess.run(["ffmpeg", "-loglevel", "error", "-i", args.wav, "-f", "s16le",
                              "-ar", str(RATE), "-ac", "1", "pipe:1"],
                             capture_output=True, check=True).stdout
        await run_session(pcm, args.gate, args.wav)
        return

    async with httpx.AsyncClient(http2=True, timeout=60) as c:
        u1 = await synth(c, "大家晚上好，今天我们继续玩这个解谜游戏。")
        u2a = await synth(c, "刚才那个机关")
        u2b = await synth(c, "我其实早就看出来了。")
        u3 = await synth(c, "好，我们去下一关看看。")
    u2 = u2a + silence(0.4) + u2b  # 句中停顿 400 ms
    timeline = (silence(0.5) + u1 + silence(2.0) + u2 + silence(45.0) + u3 + silence(1.5))
    save_wav(OUT / "timeline.wav", timeline, RATE)
    print(f"时间线 {len(timeline) / 2 / RATE:.0f}s：句1，停 2s，句2（中间停 400ms），空闲 45s 不发音频，句3")

    _, tts_lines = await asyncio.gather(run_session(timeline, args.gate, "同一会话连续三句"),
                                        tts_checks())
    print("\n".join(tts_lines))
    clip = silence(0.5) + u2 + silence(1.5)
    await run_session(clip, 300, "句2 用 300ms 门限")
    await run_session(clip, args.gate, f"句2 用 {args.gate}ms 门限")


if __name__ == "__main__":
    asyncio.run(main())
