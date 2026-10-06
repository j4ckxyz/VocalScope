# /// script
# requires-python = ">=3.10"
# dependencies = ["numpy>=1.26", "lameenc>=1.7"]
# ///
"""Generate synthetic test recordings into ./test-audio (git-ignored).

    uv run scripts/make_test_audio.py            # standard set
    uv run scripts/make_test_audio.py --long     # also a 60-minute MP3

The audio is a sung-sounding tone (harmonics, vibrato, pitch glides between
notes, breaths of noise between phrases) so waveforms look like real material
and pitch tracking has a known ground truth to be checked against. A
one-minute pair, "natural" and "corrected", is for trying the correction
indicators and the A/B comparison. Nothing here is copyrighted, so the files
can be shared freely.

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


def synth(seconds: float, seed: int = 1, style: str = "plain") -> np.ndarray:
    """Returns float32 stereo samples in [-1, 1], shape (frames, 2).

    `style` chooses how the melody is sung:

    * "plain"      exactly on the scale, with glides and vibrato (the default)
    * "natural"    as a person might: each note a little off, wandering slightly
    * "corrected"  as hard pitch correction leaves it: exactly on the scale,
                   no vibrato, and jumping between notes instantly
    """
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
    if style == "natural":
        # Every note lands up to a quarter of a semitone off, and the voice
        # wanders by a few cents while holding it.
        detune_rng = np.random.default_rng(seed + 1000)
        per_note = detune_rng.normal(0.0, 0.16, frames // step + 2).clip(-0.4, 0.4)
        filled = filled + per_note[(t / NOTE_SECONDS).astype(int)]
        wander = np.convolve(
            detune_rng.standard_normal(frames // 441 + 2), np.hanning(25) / np.hanning(25).sum(), mode="same"
        )
        filled = filled + 0.22 * np.interp(np.arange(frames), np.arange(len(wander)) * 441, wander)
    if style == "corrected":
        pitch = filled
    else:
        glide = int(0.06 * SAMPLE_RATE)
        kernel = np.hanning(glide * 2 + 1)
        kernel /= kernel.sum()
        pitch = np.convolve(filled, kernel, mode="same")

    # Vibrato that fades in over each note, about 5.5 Hz and +/- 35 cents.
    into_note = (t % NOTE_SECONDS) / NOTE_SECONDS
    vibrato = 0.35 * np.clip((into_note - 0.3) / 0.3, 0, 1) * np.sin(2 * np.pi * 5.5 * t)
    if style == "corrected":
        vibrato = 0.0
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

    # A pair for trying the A/B comparison: the same performance "as sung"
    # and "as corrected", the second starting 1.3 seconds later.
    natural = args.out / "synthetic-vocal-natural-1min.wav"
    write_wav(natural, synth(60.0, style="natural"))
    print(f"wrote {natural}")
    lead_in = np.zeros((int(1.3 * SAMPLE_RATE), 2), dtype=np.float32)
    corrected = args.out / "synthetic-vocal-corrected-1min.wav"
    write_wav(corrected, np.concatenate([lead_in, synth(60.0, style="corrected")]))
    print(f"wrote {corrected}")

    if args.long:
        long_mp3 = args.out / "synthetic-vocal-60min.mp3"
        write_mp3(long_mp3, 3600.0)
        print(f"wrote {long_mp3}")


if __name__ == "__main__":
    main()
