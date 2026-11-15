#!/usr/bin/env python3
"""Content-addressed neural TTS cache + per-scene 48 kHz AAC clip audio builder.

Subcommands:
  synth <prompts.json> <cache_dir> <out_segments.json> [--voice en-US-AndrewNeural] [--rate +4%]
  clip-audio <wav_or_NONE> <clip_duration_ms> <offset_ms> <out_clip.m4a>
  assemble <cues.json> <segments.json> <out_voiceover.m4a>
"""
from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import math
import re
import subprocess
import wave
from array import array
from pathlib import Path


def get_duration_ms(path: Path) -> int:
    out = subprocess.check_output(["afinfo", str(path)], text=True)
    m = re.search(r"estimated duration:\s*([0-9.]+)\s*sec", out)
    if not m:
        raise RuntimeError(f"Could not parse duration from afinfo for {path}")
    return int(round(float(m.group(1)) * 1000))


async def cmd_synth(args: argparse.Namespace) -> None:
    import edge_tts

    prompts = json.loads(Path(args.prompts).read_text())
    cache_dir = Path(args.cache_dir)
    cache_dir.mkdir(parents=True, exist_ok=True)
    results = []

    for item in prompts:
        seg_id = item["id"]
        text = item["text"]
        digest = hashlib.sha256(f"{args.voice}|{args.rate}|{text}".encode()).hexdigest()[:20]
        wav_path = cache_dir / f"{seg_id}_{digest}.wav"
        mp3_path = cache_dir / f"{seg_id}_{digest}.mp3"

        if wav_path.exists() and wav_path.stat().st_size > 2048:
            dur_ms = get_duration_ms(wav_path)
            print(f"  [cache hit] {seg_id:18s}: {dur_ms:5d} ms")
        else:
            for attempt in range(1, 7):
                try:
                    comm = edge_tts.Communicate(text, args.voice, rate=args.rate)
                    await comm.save(str(mp3_path))
                    if mp3_path.exists() and mp3_path.stat().st_size > 1000:
                        break
                except Exception as exc:
                    print(f"  [retry {attempt}] {seg_id}: {exc}", flush=True)
                    await asyncio.sleep(1.4 * attempt)
            else:
                raise RuntimeError(f"Failed to synthesize neural audio for {seg_id}")

            subprocess.run(
                ["afconvert", "-f", "WAVE", "-d", "LEI16@48000", "-c", "2", str(mp3_path), str(wav_path)],
                check=True,
            )
            dur_ms = get_duration_ms(wav_path)
            print(f"  [synthesized] {seg_id:16s}: {dur_ms:5d} ms", flush=True)
            await asyncio.sleep(0.85)

        results.append({"id": seg_id, "text": text, "wav": str(wav_path), "durationMs": dur_ms})

    Path(args.out).write_text(json.dumps(results, indent=2))


def cmd_assemble(args: argparse.Namespace) -> None:
    cue_data = json.loads(Path(args.cues).read_text())
    total_ms = int(cue_data["totalVideoMs"])
    cues = cue_data["cues"]
    seg_map = {s["id"]: s for s in json.loads(Path(args.segments).read_text())}

    sample_rate = 48000
    channels = 2
    total_frames = int((total_ms + 500) * sample_rate / 1000)
    master = array("h", [0]) * (total_frames * channels)

    prev_end_ms = 0
    for c in cues:
        seg_id = c["id"]
        start_ms = max(int(c["startMs"]), prev_end_ms + 180)
        seg = seg_map[seg_id]
        with wave.open(seg["wav"], "rb") as wf:
            assert wf.getframerate() == sample_rate and wf.getnchannels() == channels
            nframes = wf.getnframes()
            samples = array("h")
            samples.frombytes(wf.readframes(nframes))

        fade_frames = min(int(0.015 * sample_rate), nframes // 4)
        for f in range(fade_frames):
            gain = 0.5 * (1.0 - math.cos(math.pi * f / fade_frames))
            samples[2 * f] = int(samples[2 * f] * gain)
            samples[2 * f + 1] = int(samples[2 * f + 1] * gain)
            ri = nframes - 1 - f
            samples[2 * ri] = int(samples[2 * ri] * gain)
            samples[2 * ri + 1] = int(samples[2 * ri + 1] * gain)

        start_frame = int(start_ms * sample_rate / 1000)
        end_frame = min(start_frame + nframes, total_frames)
        usable = (end_frame - start_frame) * channels
        master[start_frame * channels : start_frame * channels + usable] = samples[:usable]
        end_ms = start_ms + int(nframes * 1000 / sample_rate)
        print(f"  Placed {seg_id:18s}: {start_ms/1000:6.2f}s -> {end_ms/1000:6.2f}s ({seg['durationMs']/1000:.2f}s)")
        prev_end_ms = end_ms

    out_m4a = Path(args.out_m4a)
    wav_out = out_m4a.with_suffix(".wav")
    with wave.open(str(wav_out), "wb") as wf:
        wf.setnchannels(channels)
        wf.setsampwidth(2)
        wf.setframerate(sample_rate)
        wf.writeframes(master[: int(total_ms * sample_rate / 1000) * channels].tobytes())

    out_m4a.unlink(missing_ok=True)
    subprocess.run(
        ["afconvert", "-f", "m4af", "-d", "aac", "-b", "192000", str(wav_out), str(out_m4a)],
        check=True,
    )


def cmd_clip_audio(args: argparse.Namespace) -> None:
    sample_rate = 48000
    channels = 2
    clip_ms = max(100, int(args.clip_duration_ms))
    offset_ms = max(0, int(args.offset_ms))
    total_frames = int(round(clip_ms * sample_rate / 1000.0))
    master = array("h", [0]) * (total_frames * channels)

    if args.wav and args.wav != "NONE" and Path(args.wav).is_file():
        with wave.open(args.wav, "rb") as wf:
            assert wf.getframerate() == sample_rate and wf.getnchannels() == channels
            nframes = wf.getnframes()
            samples = array("h")
            samples.frombytes(wf.readframes(nframes))

        fade_frames = min(int(0.015 * sample_rate), nframes // 4)
        for f in range(fade_frames):
            gain = 0.5 * (1.0 - math.cos(math.pi * f / fade_frames))
            samples[2 * f] = int(samples[2 * f] * gain)
            samples[2 * f + 1] = int(samples[2 * f + 1] * gain)
            ri = nframes - 1 - f
            samples[2 * ri] = int(samples[2 * ri] * gain)
            samples[2 * ri + 1] = int(samples[2 * ri + 1] * gain)

        start_frame = min(int(offset_ms * sample_rate / 1000), total_frames)
        end_frame = min(start_frame + nframes, total_frames)
        usable = max(0, (end_frame - start_frame) * channels)
        if usable > 0:
            master[start_frame * channels : start_frame * channels + usable] = samples[:usable]

    out_m4a = Path(args.out_m4a)
    wav_out = out_m4a.with_suffix(".clip.wav")
    with wave.open(str(wav_out), "wb") as wf:
        wf.setnchannels(channels)
        wf.setsampwidth(2)
        wf.setframerate(sample_rate)
        wf.writeframes(master.tobytes())

    out_m4a.unlink(missing_ok=True)
    subprocess.run(
        ["afconvert", "-f", "m4af", "-d", "aac", "-b", "192000", str(wav_out), str(out_m4a)],
        check=True,
    )
    wav_out.unlink(missing_ok=True)


def main() -> None:
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)

    p_syn = sub.add_parser("synth")
    p_syn.add_argument("prompts")
    p_syn.add_argument("cache_dir")
    p_syn.add_argument("out")
    p_syn.add_argument("--voice", default="en-US-AndrewNeural")
    p_syn.add_argument("--rate", default="+4%")

    p_clip = sub.add_parser("clip-audio")
    p_clip.add_argument("wav")
    p_clip.add_argument("clip_duration_ms")
    p_clip.add_argument("offset_ms")
    p_clip.add_argument("out_m4a")

    p_asm = sub.add_parser("assemble")
    p_asm.add_argument("cues")
    p_asm.add_argument("segments")
    p_asm.add_argument("out_m4a")

    args = ap.parse_args()
    if args.cmd == "synth":
        asyncio.run(cmd_synth(args))
    elif args.cmd == "clip-audio":
        cmd_clip_audio(args)
    else:
        cmd_assemble(args)


if __name__ == "__main__":
    main()
