"""Lays the voice-over lines on a ducked music bed and muxes the result into the rendered
promo videos. Placement is sample-exact on float buses; ffmpeg only decodes, normalises
and muxes.

    python -I promo/scripts/mix_audio.py
"""
import subprocess
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parents[1]
AUDIO = ROOT / "audio"
OUT = ROOT / "out"
SR = 48000
LENGTH_S = 40.2
MUSIC_GAIN = 0.42
DUCK_GAIN = 0.16
RAMP_S = 0.18
VO_GAIN = 1.0
# (file, start second)
LINES = [
    ("vo1.mp3", 4.4),
    ("vo2.mp3", 7.3),
    ("vo3.mp3", 10.0),
    ("vo4.mp3", 14.0),
    ("vo5.mp3", 20.0),
    ("vo6.mp3", 22.8),
    ("vo7.mp3", 29.2),
    ("vo8.mp3", 35.6),
]
VIDEOS = ["casecomplete-promo.mp4", "casecomplete-promo-vertical.mp4"]


def decode(path: Path) -> np.ndarray:
    raw = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", str(path), "-f", "f32le", "-ac", "1", "-ar", str(SR), "-"],
        check=True, capture_output=True,
    ).stdout
    return np.frombuffer(raw, dtype=np.float32)


def fade_edges(x: np.ndarray, ms: float = 12.0) -> np.ndarray:
    n = min(len(x), int(SR * ms / 1000))
    if n == 0:
        return x
    x = x.copy()
    ramp = np.linspace(0.0, 1.0, n, dtype=np.float32)
    x[:n] *= ramp
    x[-n:] *= ramp[::-1]
    return x


def main() -> None:
    total = int(LENGTH_S * SR)
    vo = np.zeros(total, dtype=np.float32)
    active = np.zeros(total, dtype=bool)
    for name, start in LINES:
        clip = fade_edges(decode(AUDIO / name))
        s = int(start * SR)
        e = min(total, s + len(clip))
        vo[s:e] += clip[: e - s] * VO_GAIN
        active[s:e] = True

    music = decode(AUDIO / "music.mp3")
    bed = np.zeros(total, dtype=np.float32)
    n = min(total, len(music))
    bed[:n] = music[:n]
    tail = int(1.5 * SR)
    bed[-tail:] *= np.linspace(1.0, 0.0, tail, dtype=np.float32)

    # Ducking envelope: full music gain outside speech, DUCK_GAIN under it, smoothed.
    env = np.where(active, DUCK_GAIN, MUSIC_GAIN).astype(np.float32)
    k = int(RAMP_S * SR)
    kernel = np.ones(k, dtype=np.float32) / k
    env = np.convolve(env, kernel, mode="same").astype(np.float32)
    mix = bed * env + vo
    peak = float(np.max(np.abs(mix))) or 1.0
    if peak > 0.98:
        mix *= 0.98 / peak

    stereo = np.repeat(mix[:, None], 2, axis=1).astype(np.float32).tobytes()
    raw_wav = OUT / "promo-mix-raw.wav"
    subprocess.run(
        ["ffmpeg", "-v", "error", "-y", "-f", "f32le", "-ac", "2", "-ar", str(SR), "-i", "-", str(raw_wav)],
        input=stereo, check=True,
    )
    mix_wav = OUT / "promo-mix.wav"
    subprocess.run(
        ["ffmpeg", "-v", "error", "-y", "-i", str(raw_wav), "-af", "loudnorm=I=-16:TP=-1.5:LRA=11", str(mix_wav)],
        check=True,
    )
    for video in VIDEOS:
        src = OUT / video
        if not src.exists():
            print("missing", src)
            continue
        dst = OUT / video.replace(".mp4", "-sound.mp4")
        subprocess.run(
            ["ffmpeg", "-v", "error", "-y", "-i", str(src), "-i", str(mix_wav), "-map", "0:v:0", "-map", "1:a:0",
             "-c:v", "copy", "-c:a", "aac", "-b:a", "192k", "-shortest", "-movflags", "+faststart", str(dst)],
            check=True,
        )
        print(dst)


if __name__ == "__main__":
    main()
