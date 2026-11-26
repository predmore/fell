"""Measure list positions, fading selection, and competing ink from a recording.

Run with NumPy and ffmpeg. A reference frame is found before the archive by
looking for the complete selection band. Later frames are compared with the
before and settled glyphs in the first text column.
"""

import argparse
import csv
from itertools import combinations
from pathlib import Path
import subprocess

import numpy as np

ROW = 68
WIDTH = 220
HEIGHT = 826                   # 900 px window, minus the 44 px header and 30 px footer
BAND_X = 10 - 8
SCAN = slice(16 - 8, None)


def frames(path):
    command = [
        "ffmpeg", "-v", "error", "-i", str(path), "-vf",
        f"crop={WIDTH}:{HEIGHT}:8:44,format=gray", "-fps_mode", "passthrough",
        "-f", "rawvideo", "-",
    ]
    data = subprocess.check_output(command)
    return np.frombuffer(data, dtype=np.uint8).reshape(-1, HEIGHT, WIDTH)


def ink(frames, polarity):
    background = frames[..., BAND_X:BAND_X + 1].astype(np.float32)
    return np.maximum(0, polarity * (frames[..., SCAN].astype(np.float32) - background))


def centered(field):
    return field - field.mean(axis=1, keepdims=True)


def shifted_field(field, offset):
    result = np.zeros_like(field)
    result[offset:] = field[:len(field) - offset]
    return result


def tracker(reference, max_shift):
    """Match survivor glyphs by their shape, independent of opacity."""
    target = centered(reference)
    size = 2 * len(target)
    target_fft = np.fft.rfft(target, n=size, axis=0).conj()
    target_energy = np.r_[0, (target * target).sum(axis=1).cumsum()]
    offsets = np.arange(max_shift + 1)

    def track(field):
        signal = centered(field)
        signal_fft = np.fft.rfft(signal, n=size, axis=0)
        cross = np.fft.irfft((signal_fft * target_fft).sum(axis=1), n=size)[offsets]
        energy = np.r_[0, (signal * signal).sum(axis=1).cumsum()]
        norm = np.sqrt((energy[-1] - energy[offsets]) * target_energy[len(target) - offsets])
        scores = cross / np.maximum(1, norm)
        # The highest match should have sustained ink in the expected location.
        offset = int(np.argmax(scores))
        return offset, float(scores[offset])

    return track


def competing_ink(signal, *patterns):
    """Count y positions with independently measured strokes from two rows."""
    observed = centered(signal)
    energy = (observed * observed).sum(axis=1)
    shapes = [centered(pattern) for pattern in patterns]
    lengths = [np.sqrt((shape * shape).sum(axis=1)) for shape in shapes]
    units = [shape / np.maximum(1, length[:, None]) for shape, length in zip(shapes, lengths)]
    matches = [(observed * unit).sum(axis=1).clip(min=0) for unit in units]
    best_single = np.max(np.square(matches), axis=0)
    detected = np.zeros(len(signal), dtype=bool)
    for i, j in combinations(range(len(patterns)), 2):
        a = patterns[i]
        b = patterns[j]
        a_only = (a >= 32) & (b <= 3)
        b_only = (b >= 32) & (a <= 3)
        a_hits = ((signal >= 10) & a_only).sum(axis=1)
        b_hits = ((signal >= 10) & b_only).sum(axis=1)
        overlap = (units[i] * units[j]).sum(axis=1)
        separation = np.maximum(0.01, 1 - overlap ** 2)
        a_weight = (matches[i] - overlap * matches[j]) / separation
        b_weight = (matches[j] - overlap * matches[i]) / separation
        fit = a_weight * matches[i] + b_weight * matches[j]
        detected |= (
            (a_hits >= 5) & (b_hits >= 5)
            & (a_weight >= 0.12 * lengths[i]) & (b_weight >= 0.12 * lengths[j])
            & (fit >= 0.78 * energy) & (fit - best_single >= 0.16 * energy)
        )
    return int(detected.sum())


def measure(path, count, screenshot=None):
    raw = frames(path)
    band = raw[..., BAND_X].astype(np.float32)
    background = np.median(band[-1, 4 * ROW:8 * ROW])
    selection = np.median(band[-1, 8:ROW - 8])
    polarity = 1 if background < 128 else -1
    fill = ((band - background) / (selection - background)).clip(0, 1)
    fully_selected = np.flatnonzero((fill[:, :count * ROW] > 0.9).mean(axis=1) >= 0.98)
    assert len(fully_selected), f"{path.name}: expected {count} selected rows"
    first = fully_selected[0]
    field = ink(raw, polarity)
    old, final = field[first], field[-1]
    hold = np.abs(field[first:] - old).mean(axis=(1, 2)) < 0.55
    start = int(np.flatnonzero(~hold)[0] + first)
    assert start > first, f"{path.name}: no held selection"
    track = tracker(final, count * ROW)
    assert track(old)[0] == count * ROW, f"{path.name}: the before/after row counts differ"
    last_moving = start
    result = []
    # Positive control: blend two different, vertically aligned reference glyphs.
    assert competing_ink(0.5 * old + 0.5 * final, old, final) > 0
    for index, current in enumerate(field[start - 1:], start=start - 1):
        old_band = fill[index, :count * ROW]
        dy, match = track(current)
        assert match >= 0.70, (path.name, index, dy, match)
        candidates = [old, shifted_field(final, dy)]
        if dy:
            candidates.append(final)
        layers = competing_ink(current, *candidates)
        partial = int(((old_band > 0.08) & (old_band < 0.92)).sum())
        if index >= start and (dy or partial or np.abs(current - final).mean() > 0.55):
            last_moving = index
        result.append((index, round(index / 60, 3), dy, round(match, 3), layers, partial))
    with path.with_suffix(".csv").open("w", newline="") as output:
        out = csv.writer(output, lineterminator="\n")
        out.writerow(("frame", "seconds", "survivor_offset_px", "tracking_match",
                      "scanlines_with_competing_ink", "partial_selection_px"))
        out.writerows(result)
    transition = [record for record in result if start <= record[0] <= last_moving]
    assert transition, path
    peak = max(x[2] for x in transition)
    overlap = sum(x[4] > 0 for x in transition)
    first_partial = next((x for x in transition if x[5] > 0), transition[0])
    print(f"{path.name}: archive frames {start}–{last_moving}, max {peak} px, "
          f"{overlap}/{len(transition)} frames with competing ink, "
          f"{first_partial[5]} px partial selection at frame {first_partial[0]}")
    if screenshot:
        subprocess.run([
            "ffmpeg", "-v", "error", "-y", "-i", str(path), "-vf",
            f"select=eq(n\\,{first_partial[0]})", "-frames:v", "1", str(screenshot),
        ], check=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", type=Path)
    parser.add_argument("count", type=int)
    parser.add_argument("--screenshot", type=Path)
    args = parser.parse_args()
    measure(args.path, args.count, args.screenshot)
