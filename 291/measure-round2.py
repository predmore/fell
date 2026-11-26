"""Measure batch archive motion from the lossless 900 px tall demo captures."""

import argparse
import csv
from pathlib import Path
import subprocess

import numpy as np

from measure import centered, competing_ink

ROW = 68
WIDTH = 408
HEIGHT = 826
BAND_X = 2
TEXT_X = 8


def decode(path):
    data = subprocess.check_output([
        "ffmpeg", "-v", "error", "-i", str(path), "-vf",
        f"crop={WIDTH}:{HEIGHT}:8:44,format=gray", "-fps_mode", "passthrough",
        "-f", "rawvideo", "-",
    ])
    times = subprocess.check_output([
        "ffprobe", "-v", "error", "-select_streams", "v:0",
        "-show_entries", "frame=best_effort_timestamp_time", "-of", "csv=p=0", str(path),
    ], text=True)
    frames = np.frombuffer(data, dtype=np.uint8).reshape(-1, HEIGHT, WIDTH)
    timestamps = np.array([float(line.split(",")[0]) for line in times.splitlines() if line])
    assert len(frames) == len(timestamps)
    return frames, timestamps


def reference_frames(raw, timestamps, marked_tops):
    band = raw[..., BAND_X]
    colors, counts = np.unique(band[-1], return_counts=True)
    background, selection = colors[np.argsort(counts)[-2:][::-1]]
    # Require all four marked bands simultaneously before following the archive.
    marks = np.stack([
        (band[:, top + 8:top + ROW - 8] == selection).mean(axis=1)
        for top in marked_tops
    ])
    marked = np.flatnonzero((marks > 0.9).all(axis=0))
    assert len(marked), "expected four marked slots"
    # Each capture holds the selection, archives, and records the settled
    # state for 1.25 s. Choose the held reference near the end of that pause.
    before = marked[np.argmin(np.abs(timestamps[marked] - (timestamps[-1] - 1.6)))]
    delta = np.abs(raw.astype(np.int16) - raw[before]).mean(axis=(1, 2))
    first = before + np.flatnonzero(delta[before:] > 0.15)[0]
    delta_from_settled = np.abs(raw.astype(np.int16) - raw[-1]).mean(axis=(1, 2))
    settled = np.flatnonzero(delta_from_settled[first:] > 0.15)[-1] + first + 1
    assert first > before and settled < len(raw) - 15, (before, first, settled, len(raw))
    polarity = 1 if background < 128 else -1
    ink = np.maximum(0, polarity * (
        raw[..., TEXT_X:].astype(np.float32) - band[..., None]
    ))
    return ink, first, settled


def row_tracker(final, top, offsets):
    reference = centered(final[top:top + ROW])
    reference_power = np.square(reference).sum(axis=1)
    reference_norm = np.sqrt(reference_power.sum())
    candidates = (top + offsets[:, None]) + np.arange(ROW)

    def track(frame):
        fields = frame[candidates]
        fields -= fields.mean(axis=-1, keepdims=True)
        projection = np.einsum("fij,ij->f", fields, reference)
        magnitudes = np.sqrt(np.einsum("fij,fij->f", fields, fields))
        scores = projection / np.maximum(1, magnitudes * reference_norm)
        best = np.argmax(scores)
        # A row slicing through a slot boundary loses a run of glyph scanlines.
        # Count missing scanlines with at least four strong reference pixels.
        strong = (reference > 24).sum(axis=1) >= 4
        retained = (fields[best] * reference).sum(axis=1) / np.maximum(1, reference_power)
        missing = np.count_nonzero(strong & (retained < 0.6))
        return int(offsets[best]), float(scores[best]), int(missing)

    return track


def placement(signal, reference, top, dy):
    first = max(0, top + dy)
    last = min(len(signal), top + dy + ROW)
    if last > first:
        signal[first:last] = reference[first - dy:last - dy]


def measure(path, scenario):
    raw, timestamps = decode(path)
    if scenario == "mixed":
        marked_tops = [index * ROW for index in (0, 2, 4, 6)]
        tracked = [(index * ROW, np.arange(0, (index + 1) * ROW + 1)) for index in range(3)]
    else:
        marked_tops = [HEIGHT - index * ROW for index in range(4, 0, -1)]
        tracked = [(HEIGHT - index * ROW, np.arange(-3 * ROW, 1)) for index in range(4, 0, -1)]
    ink, first, settled = reference_frames(raw, timestamps, marked_tops)
    before, final = ink[first - 1], ink[-1]
    trackers = [row_tracker(final, top, offsets) for top, offsets in tracked]
    outgoing = np.zeros_like(final)
    for top in marked_tops:
        placement(outgoing, before, top, 0)

    # Positive controls for overlapping text and for a clipped moving row.
    assert competing_ink(0.5 * before + 0.5 * final, before, final) > 0
    clipped = final.copy()
    top = tracked[0][0]
    clipped[top + 15:top + 45] = 0
    assert trackers[0](clipped)[2] > 0

    result = []
    for index in range(first, settled + 1):
        current = ink[index]
        matches = [track(current) for track in trackers]
        offsets, scores, missing = zip(*matches)
        sliding = np.zeros_like(final)
        if scenario == "mixed":
            arrivals = final.copy()
            arrivals[:3 * ROW] = 0
            for (top, _), dy in zip(tracked, offsets):
                placement(sliding, final, top, dy)
        else:
            assert len(set(offsets)) == 1, (path, index, offsets)
            dy = offsets[0]
            arrivals = np.zeros_like(final)
            sliding[:HEIGHT + dy] = final[-dy:]

        competing = competing_ink(current, outgoing, sliding, arrivals)
        result.append([
            index, round(1000 * (timestamps[index] - timestamps[first]), 2),
            *offsets, round(min(scores), 3), sum(missing), competing,
        ])
    assert any(result[0][2:2 + len(tracked)]), (path, "first frame already settled")
    assert not any(result[-1][2:2 + len(tracked)]), (path, "last frame is still moving")
    for previous, current in zip(result, result[1:]):
        assert all(abs(a) >= abs(b) for a, b in zip(previous[2:-3], current[2:-3])), (
            path, "offset changed direction", previous, current
        )
    with path.with_suffix(".csv").open("w", newline="") as output:
        out = csv.writer(output, lineterminator="\n")
        labels = ("old_1", "old_3", "old_5") if scenario == "mixed" else (
            "fourth_from_end", "third_from_end", "second_from_end", "last",
        )
        out.writerow(("frame", "ms_from_first_change", *(name + "_dy_px" for name in labels),
                      "min_glyph_match", "missing_tracked_scanlines", "competing_scanlines"))
        out.writerows(result)
    for row in result:
        assert row[-3] >= 0.85 and row[-2:] == [0, 0], (path, row)
    print(f"{path.name}: {len(result)} frames, initial offsets {result[0][2:-3]}, "
          f"lowest glyph match {min(row[-3] for row in result):.3f}, 0 missing or competing scanlines")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", type=Path)
    parser.add_argument("scenario", choices=["mixed", "bottom"])
    options = parser.parse_args()
    measure(options.path, options.scenario)
