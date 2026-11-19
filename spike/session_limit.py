# /// script
# requires-python = ">=3.11"
# dependencies = ["httpx[http2]>=0.27", "websockets>=15"]
# ///
"""转写会话到 10 分钟上限时会发生什么：有没有 goAway、什么时候断、断开码是什么。

每隔 --every 秒说一句（只在说话时发音频），一直跑到 --minutes 分钟。
第一次用 120 秒间隔时，连接在空闲约 1 分钟后被关闭（code=1008），见 out/idle_close.log。
用法：uv run spike/session_limit.py [--every 40] [--minutes 11.5]
"""
import argparse
import asyncio
import base64
import json
import time

import httpx
import websockets

from latency_bench import API, STT_MODEL, WS_URL, api_key, resample, tts, voiced_span

RATE = 16000


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--every", type=float, default=40)
    ap.add_argument("--minutes", type=float, default=11.5)
    args = ap.parse_args()
    async with httpx.AsyncClient(http2=True, timeout=60) as c:
        r = await tts(c, "gemini-3.8-flash-lite-tts", "现在测试一下会话还在不在。", voice="Puck")
    pcm = resample(r["pcm"], 24000, RATE)
    a, b = voiced_span(pcm, RATE)
    clip = pcm[int(a * RATE) * 2: int((b + 0.6) * RATE) * 2]

    setup = {"setup": {"model": STT_MODEL, "generationConfig": {"responseModalities": ["TEXT"]},
                       "inputAudioTranscription": {"languageCodes": []}}}
    t0 = time.perf_counter()
    last_audio = [t0]

    def log(*x):
        print(f"[{(time.perf_counter() - t0) / 60:5.2f} min]", *x, flush=True)

    async with websockets.connect(f"{WS_URL}?key={api_key()}", max_size=None, open_timeout=20) as ws:
        await ws.send(json.dumps(setup))
        log("setup:", (await ws.recv())[:80])

        async def rx():
            try:
                async for raw in ws:
                    m = json.loads(raw)
                    sc = m.get("serverContent") or {}
                    if "inputTranscription" in sc:
                        log("定稿:", sc["inputTranscription"].get("text", ""))
                    elif "interimInputTranscription" not in sc and "voiceActivity" not in m:
                        log("消息:", json.dumps(m, ensure_ascii=False)[:300])
            except websockets.ConnectionClosed as e:
                log(f"连接关闭 code={e.code} reason={e.reason!r}"
                    f"  距上次发音频 {time.perf_counter() - last_audio[0]:.0f}s")

        rx_task = asyncio.create_task(rx())
        try:
            for k in range(int(args.minutes * 60 // args.every) + 1):
                target = t0 + k * args.every
                await asyncio.sleep(max(0, target - time.perf_counter()))
                if rx_task.done():
                    break
                log(f"第 {k + 1} 句开始发送")
                for i in range(0, len(clip), 3200):
                    await ws.send(json.dumps({"realtimeInput": {"audio": {
                        "data": base64.b64encode(clip[i:i + 3200]).decode(),
                        "mimeType": f"audio/pcm;rate={RATE}"}}}))
                    await asyncio.sleep(0.1)
                await ws.send(json.dumps({"realtimeInput": {"audioStreamEnd": True}}))
                last_audio[0] = time.perf_counter()
            await asyncio.wait_for(rx_task, 30)
        except (websockets.ConnectionClosed, asyncio.TimeoutError) as e:
            log("结束:", type(e).__name__, e)
    log("完成")


if __name__ == "__main__":
    asyncio.run(main())
