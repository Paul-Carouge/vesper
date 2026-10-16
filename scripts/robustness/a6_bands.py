"""Level per band and per second of each audio stream of a file.

usage: a6_bands.py <file.mka>

Bands: the 440 Hz tone ("firefox"), the 1000 Hz tone (a stream on another
sink) and 3-8 kHz (the white noise; the tones have nothing there). dBFS of
the band's RMS, -99 for nothing.
"""
import json
import subprocess
import sys

import numpy as np

RATE = 48_000


def streams(path):
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-show_entries", "stream_tags=title", "-of", "json", path],
        capture_output=True, check=True, text=True,
    ).stdout
    return [s.get("tags", {}).get("title", "?") for s in json.loads(out)["streams"]]


def samples(path, index):
    raw = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", path, "-map", f"0:a:{index}", "-ac", "1",
         "-ar", str(RATE), "-f", "f32le", "-"],
        capture_output=True, check=True,
    ).stdout
    return np.frombuffer(raw, dtype="<f4")


def band_db(spectrum, freqs, lo, hi):
    power = np.sum(np.abs(spectrum[(freqs >= lo) & (freqs < hi)]) ** 2)
    # Parseval: band RMS of the one-second window.
    rms = np.sqrt(2 * power) / RATE
    return max(-99.0, 20 * np.log10(rms)) if rms > 0 else -99.0


def main():
    path = sys.argv[1]
    for i, title in enumerate(streams(path)):
        x = samples(path, i)
        rows = []
        for w in range(len(x) // RATE):
            spec = np.fft.rfft(x[w * RATE:(w + 1) * RATE])
            freqs = np.fft.rfftfreq(RATE, 1 / RATE)
            rows.append((band_db(spec, freqs, 430, 450), band_db(spec, freqs, 990, 1010),
                         band_db(spec, freqs, 3000, 8000)))
        print(f"{title}:")
        print("  s   440Hz  1000Hz  noise")
        for w, (a, b, c) in enumerate(rows):
            print(f"  {w:<3d} {a:6.1f}  {b:6.1f}  {c:6.1f}")


main()
