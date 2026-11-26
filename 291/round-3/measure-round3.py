"""Measure survivor displacement and both lines of stationary ink from each video frame.

Run with NumPy, FFmpeg and FFprobe. Pass the MP4, its pre-key PNG (a locator
for the last held video frame), and the scenario. The settled video frame
supplies survivor and arrival glyphs. The immediately preceding *video* frame
supplies the time origin and the departing glyphs.
"""
import argparse
import csv
from pathlib import Path
import subprocess

import numpy as np

WIDTH = 422
HEIGHT = 826
ROW = 68
TEXT = slice(8, 400)
LINE_REGIONS = (slice(12, 35), slice(35, 58))


def decode(path):
    data = subprocess.check_output([
        "ffmpeg", "-v", "error", "-threads", "2", "-i", str(path),
        "-vf", f"crop={WIDTH}:{HEIGHT + 20}:8:44,format=gray",
        "-fps_mode", "passthrough", "-f", "rawvideo", "-",
    ])
    return np.frombuffer(data, np.uint8).reshape(-1, HEIGHT + 20, WIDTH)


def times(path):
    output = subprocess.check_output([
        "ffprobe", "-v", "error", "-select_streams", "v:0",
        "-show_entries", "frame=best_effort_timestamp_time", "-of", "csv=p=0",
        str(path),
    ], text=True)
    return np.asarray([float(line.split(",")[0]) for line in output.splitlines() if line])


def distance(a, b):
    return np.abs(a.astype(np.int16) - b).mean(axis=(1, 2))


def frame_range(raw, reference, timestamps):
    if isinstance(reference, int):
        before = reference
    else:
        error = distance(raw, reference.astype(np.int16))
        near_key = timestamps < timestamps[-1] - 0.4
        assert np.any(near_key)
        tolerance = max(0.05, float(error[near_key].min()) + 0.025)
        assert tolerance < 0.45, f"pre-key still was not present in the recording ({tolerance})"
        before = int(np.flatnonzero(near_key & (error <= tolerance))[-1])
    assert 0 <= before < len(raw) - 15
    row_error = distance(raw[before:, :HEIGHT], raw[-1, :HEIGHT].astype(np.int16))
    settled = before + int(np.flatnonzero(row_error > 0.04)[-1]) + 1
    assert before < settled < len(raw) - 15
    return before, settled


def glyphs(raw):
    # Remove the local fill (including a selection band) and the mean of each
    # scan line. Works for either foreground polarity.
    values = raw[:, :HEIGHT, TEXT].astype(np.float32)
    return values - values.mean(axis=-1, keepdims=True)


def slot(n):
    return HEIGHT - n * ROW


def row_pairs(scenario):
    if scenario == "mixed":
        return [(1 * ROW, 0), (3 * ROW, ROW), (5 * ROW, 2 * ROW)]
    if scenario == "bottom-4":
        return [(slot(n + 4), slot(n)) for n in (4, 3, 2, 1)]
    if scenario == "bottom-10":
        return [(slot(n + 10), slot(n)) for n in (2, 1)]
    return [(slot(n + 4), slot(n)) for n in (8, 7, 6, 5)] + [
        (slot(7), slot(4)), (slot(5), slot(3)), (slot(3), slot(2)), (slot(1), slot(1)),
    ]


def row_template(image, top):
    # Use the two text lines and omit the row borders.
    return image[top + 9:top + 61]


def make_tracker(settled, old, final):
    needle = row_template(settled, final)
    size = 1 << (HEIGHT + len(needle) - 1).bit_length()
    template = np.fft.rfft(needle.astype(np.float64), n=size, axis=0).conj()
    energy = np.square(needle).sum()
    low = min(old, final) + 9
    high = max(old, final) + 9

    def track(signal_fft, energy_prefix):
        dot = np.fft.irfft((signal_fft * template).sum(axis=1), n=size)[low:high + 1]
        powers = energy_prefix[low + len(needle):high + len(needle) + 1] - energy_prefix[low:high + 1]
        scores = dot / np.maximum(1.0, np.sqrt(powers * energy))
        # FFT rounding in a blank stripe is not a glyph match.
        scores[powers < 0.05 * energy] = -np.inf
        i = int(np.argmax(scores))
        return low + i - 9, float(scores[i])

    return track


def line_alpha(frame, reference, top):
    result = []
    for line in LINE_REGIONS:
        region = slice(top + line.start, top + line.stop)
        target = reference[region]
        current = frame[region]
        energy = float(np.square(target).sum())
        dot = float(np.multiply(target, current).sum())
        residual = float(np.square(current).sum())
        match = dot / max(1.0, np.sqrt(energy * residual))
        # A different row passing the fixed slot contributes zero to this
        # template unless at least half the reference shape is present.
        result.append(float(np.clip(dot / max(1.0, energy), 0, 1)) if match >= 0.5 else 0.0)
    return result


def measure(path, still_path, scenario):
    raw = decode(path)
    timestamps = times(path)
    assert len(raw) == len(timestamps)
    locator = still_path if isinstance(still_path, int) else decode(still_path)[0]
    before, settled = frame_range(raw, locator, timestamps)
    signals = glyphs(raw[before:settled + 1])
    pairs = row_pairs(scenario)
    trackers = [make_tracker(signals[-1], old, final) for old, final in pairs]
    arrivals = [3 * ROW, 4 * ROW, 5 * ROW, 6 * ROW] if scenario == "mixed" else []
    ghost_tops = {
        "mixed": [0, 2 * ROW, 4 * ROW, 6 * ROW],
        "bottom-4": [slot(n) for n in (4, 3, 2, 1)],
        "bottom-10": [slot(n) for n in (10, 7, 4, 1)],
        "above": [slot(n) for n in (8, 6, 4, 2)],
    }[scenario]
    records = []
    for i, current in enumerate(signals):
        spectrum = np.fft.rfft(current.astype(np.float64), n=1024, axis=0)
        prefix = np.r_[0, np.cumsum(np.square(current).sum(axis=1), dtype=np.float64)]
        positions = [track(spectrum, prefix) for track in trackers]
        old_alpha = [alpha for top in ghost_tops for alpha in line_alpha(current, signals[0], top)]
        new_alpha = [alpha for top in arrivals for alpha in line_alpha(current, signals[-1], top)]
        records.append((positions, old_alpha, new_alpha))

    baseline = [top for (top, _) in records[0][0]]
    assert baseline == [old for old, _ in pairs], (path, baseline, pairs)
    assert choices_match(records[0][0]) >= 0.77, (path, records[0][0])
    assert [top for (top, _) in records[-1][0]] == [final for _, final in pairs]
    assert choices_match(records[-1][0]) >= 0.99
    assert min(choices_match(record[0]) for record in records) >= 0.85
    group_size = min(4, len(pairs)) if scenario != "mixed" else 0
    for positions, _, _ in records:
        shared = [top - old for (top, _), old in zip(positions[:group_size], baseline)]
        assert not shared or max(shared) - min(shared) <= 1, (path, shared)

    with path.with_suffix(".csv").open("w", newline="") as output:
        labels = [
            f"survivor_{i + 1}_{suffix}" for i in range(len(pairs))
            for suffix in ("y_before_delta_px", "dy_to_final_px", "glyph_match")
        ]
        ink_labels = [
            f"{kind}_y{top}_{line}_ink" for kind, tops in (
                ("departing", ghost_tops), ("arriving", arrivals))
            for top in tops for line in ("sender", "subject")
        ]
        writer = csv.writer(output, lineterminator="\n")
        writer.writerow(["video_frame", "ms_from_pre_key_frame", *labels, *ink_labels])
        for i, (positions, old_alpha, new_alpha) in enumerate(records, before):
            motion = [
                value for (top, score), base, (_, final) in zip(positions, baseline, pairs)
                for value in (top - base, top - final, round(score, 3))
            ]
            writer.writerow([
                i, round(1000 * (timestamps[i] - timestamps[before]), 2), *motion,
                *(round(alpha, 3) for alpha in (*old_alpha, *new_alpha)),
            ])
    first = records[1][0]
    delta = [y - old for (y, _), old in zip(first, baseline)]
    print(f"{path.name}: pre-key {before}, {settled - before + 1} frames; "
          f"first displacement {delta} px; first glyph match {choices_match(first):.3f}; "
          f"min glyph match {min(choices_match(record[0]) for record in records):.3f}")


def choices_match(positions):
    return min(score for _, score in positions)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mp4", type=Path)
    parser.add_argument("before", help="pre-key PNG or frame number from a recorded CSV")
    parser.add_argument("scenario", choices=["bottom-4", "bottom-10", "mixed", "above"])
    args = parser.parse_args()
    before = int(args.before) if args.before.isdigit() else Path(args.before)
    measure(args.mp4, before, args.scenario)
