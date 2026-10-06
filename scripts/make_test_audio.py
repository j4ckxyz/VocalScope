# /// script
# requires-python = ">=3.10"
# dependencies = ["numpy>=1.26", "lameenc>=1.7"]
# ///
"""Generate synthetic test recordings into ./test-audio (git-ignored).

    uv run scripts/make_test_audio.py            # standard set
    uv run scripts/make_test_audio.py --long     # also a 60-minute MP3

The audio is a sung-sounding tone (harmonics, vibrato, pitch glides between
notes, breaths of noise between phrases) so waveforms look like real material
and, from v0.2.0, pitch tracking has a known ground truth to be checked
against. Nothing here is copyrighted, so the files can be shared freely.

WAV and MP3 are written directly; FLAC and M4A are produced with macOS
`afconvert` when it is available.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import wave
from pathlib import Path

import lameenc
import numpy as np

SAMPLE_RATE = 44_100
# A minor-ish melody, MIDI note numbers; 0 is a rest.
MELODY = [57, 60, 62, 64, 62, 60, 57, 0, 64, 65, 67, 69, 67, 64, 62, 0]
NOTE_SECONDS = 0.75


def midi_to_hz(note: float) -> float:
    return 440.0 * 2.0 ** ((note - 69.0) / 12.0)


def synth(seconds: float, seed: int = 1) -> np.ndarray:
    """Returns float32 stereo samples in [-1, 1], shape (frames, 2)."""
    rng = np.random.default_rng(seed)
    frames = int(seconds * SAMPLE_RATE)
    t = np.arange(frames) / SAMPLE_RATE

    # Target pitch per sample, then smooth it so notes glide into each other.
    note_index = (t / NOTE_SECONDS).astype(int) % len(MELODY)
    notes = np.array(MELODY, dtype=np.float64)[note_index]
    voiced = notes > 0
    held = np.where(voiced, notes, np.nan)
    # Carry the previous note through rests so the glide has somewhere to start.
    last = 57.0
    filled = np.empty(frames)
    step = int(NOTE_SECONDS * SAMPLE_RATE)
    for start in range(0, frames, step):
        value = held[start]
        if not np.isnan(value):
            last = value
        filled[start : start + step] = last
    glide = int(0.06 * SAMPLE_RATE)
    kernel = np.hanning(glide * 2 + 1)
    kernel /= kernel.sum()
    pitch = np.convolve(filled, kernel, mode="same")

    # Vibrato that fades in over each note, about 5.5 Hz and +/- 35 cents.
    into_note = (t % NOTE_SECONDS) / NOTE_SECONDS
    vibrato = 0.35 * np.clip((into_note - 0.3) / 0.3, 0, 1) * np.sin(2 * np.pi * 5.5 * t)
    freq = 440.0 * 2.0 ** ((pitch + vibrato - 69.0) / 12.0)
    phase = 2 * np.pi * np.cumsum(freq) / SAMPLE_RATE

    voice = sum(np.sin(k * phase) / k**1.4 for k in range(1, 7))
    envelope = np.clip(np.minimum(into_note / 0.05, (1 - into_note) / 0.12), 0, 1) * voiced
    # Slow phrase-level dynamics so the overview is not a solid block.
    dynamics = 0.55 + 0.45 * np.sin(2 * np.pi * t / 23.0) ** 2
    breath = rng.standard_normal(frames) * 0.02 * (~voiced)
    mono = (voice * envelope * dynamics * 0.32 + breath).astype(np.float32)

    # A touch of stereo difference so the file is genuinely stereo.
    delay = 37
    right = np.concatenate([np.zeros(delay, dtype=np.float32), mono[:-delay]]) * 0.92
    return np.stack([mono, right], axis=1)


def to_pcm16(samples: np.ndarray) -> bytes:
    return (np.clip(samples, -1, 1) * 32767).astype("<i2").tobytes()


def write_wav(path: Path, samples: np.ndarray) -> None:
    with wave.open(str(path), "wb") as out:
        out.setnchannels(samples.shape[1])
        out.setsampwidth(2)
        out.setframerate(SAMPLE_RATE)
        out.writeframes(to_pcm16(samples))


def write_mp3(path: Path, seconds: float, bitrate: int = 192, chunk_seconds: float = 60.0) -> None:
    """Encodes in chunks so a long file never needs to be held in memory."""
    encoder = lameenc.Encoder()
    encoder.set_bit_rate(bitrate)
    encoder.set_in_sample_rate(SAMPLE_RATE)
    encoder.set_channels(2)
    encoder.set_quality(5)
    encoder.silence()
    whole_chunks = int(seconds // chunk_seconds)
    with path.open("wb") as out:
        for index in range(whole_chunks):
            out.write(encoder.encode(to_pcm16(synth(chunk_seconds, seed=index + 1))))
        remainder = seconds - whole_chunks * chunk_seconds
        if remainder > 0:
            out.write(encoder.encode(to_pcm16(synth(remainder, seed=whole_chunks + 1))))
        out.write(encoder.flush())


def afconvert(source: Path, target: Path, *args: str) -> bool:
    if shutil.which("afconvert") is None:
        return False
    result = subprocess.run(["afconvert", str(source), str(target), *args], capture_output=True)
    return result.returncode == 0


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--long", action="store_true", help="also write a 60-minute MP3")
    parser.add_argument("--out", type=Path, default=Path("test-audio"))
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)

    song = synth(240.0)
    wav = args.out / "synthetic-vocal-4min.wav"
    write_wav(wav, song)
    print(f"wrote {wav}")

    mp3 = args.out / "synthetic-vocal-4min.mp3"
    write_mp3(mp3, 240.0)
    print(f"wrote {mp3}")

    for name, extra in (
        ("synthetic-vocal-4min.flac", ("-f", "flac", "-d", "flac")),
        ("synthetic-vocal-4min.m4a", ("-f", "m4af", "-d", "aac", "-b", "192000")),
    ):
        target = args.out / name
        print(f"wrote {target}" if afconvert(wav, target, *extra) else f"skipped {name} (afconvert unavailable)")

    if args.long:
        long_mp3 = args.out / "synthetic-vocal-60min.mp3"
        write_mp3(long_mp3, 3600.0)
        print(f"wrote {long_mp3}")


if __name__ == "__main__":
    main()
