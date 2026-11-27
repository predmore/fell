"""Measure list glyphs in the frames of a lossless headless Sway recording."""

import argparse
import csv
from pathlib import Path
import subprocess

import numpy as np


ROW = 68
VIEW = 826
CAPTURE_CROP = f"345:{VIEW}:16:44"
TEXT = slice(12, 56)
LINES = (slice(12, 35), slice(35, 56))


def decode(video):
    pixels = subprocess.check_output([
        "ffmpeg", "-nostdin", "-v", "error", "-threads", "2", "-i", str(video),
        "-vf", f"format=gray,crop={CAPTURE_CROP}", "-threads", "1",
        "-fps_mode", "passthrough", "-pix_fmt", "gray", "-f", "rawvideo", "-",
    ])
    return np.frombuffer(pixels, np.uint8).reshape(-1, VIEW, 345)


def glyphs(image):
    return np.diff(image.astype(np.int16), axis=1)


def blank_gap(ink):
    # Use glyph edges in the text column, excluding the selection border
    # and right-aligned date. Keep the viewport padding as a separate measure.
    occupied = np.flatnonzero((np.abs(ink) > 10).sum(axis=1) >= 5)
    if len(occupied) < 2:
        return VIEW, VIEW
    return int((np.diff(occupied) - 1).max()), int(VIEW - 1 - occupied[-1])


def match_at(reference, sample):
    target, current = reference.astype(np.float32), sample.astype(np.float32)
    energy = np.vdot(target, target)
    dot = np.vdot(target, current)
    score = dot / max(1.0, np.sqrt(energy * np.vdot(current, current)))
    return float(dot / max(1.0, energy)) if score >= 0.55 else 0.0


def track(reference, current):
    windows = np.lib.stride_tricks.sliding_window_view(current, len(reference), axis=0)
    y, x = np.nonzero(np.abs(reference) > 18)
    target = reference[y, x].astype(np.float32)
    samples = windows[:, x, y].astype(np.float32)
    dot = samples @ target
    score = dot / np.maximum(1.0, np.linalg.norm(samples, axis=1) * np.linalg.norm(target))
    i = int(np.argmax(score))
    return i - TEXT.start if score[i] >= 0.98 and dot[i] >= 0.3 * np.vdot(target, target) else None


def region(ink, top, part):
    return ink[top + part.start : top + part.stop]


def analyze(video, flow, keys):
    frames = decode(video)
    original = frames[keys[0]]
    settled = glyphs(frames[-1])
    changed = np.flatnonzero(np.any(frames[1:] != frames[:-1], axis=(1, 2))) + 1
    end = max(int(changed[-1]), keys[-1] + 1)
    first_ink = glyphs(original)
    probes = []
    if flow == "reversal":
        for number in (12, 11):
            top = VIEW - number * ROW
            template = region(first_ink, top, TEXT)
            before_each_key = [track(template, glyphs(frames[index])) for index in keys]
            probes.append((template, before_each_key))
    rows = []
    for index in range(keys[0], end + 1):
        ink = glyphs(frames[index])
        key = max(i for i, frame in enumerate(keys) if index >= frame)
        gap, bottom = blank_gap(ink)
        row = {
            "frame": index,
            "key_number": key + 1,
            "ms_from_pre_key_frame": round((index - keys[key]) * 1000 / 60, 1),
            "pre_key": index == keys[key],
            "list_changed": index in changed,
            "largest_blank_gap_px": gap,
            "trailing_blank_px": bottom,
        }
        if probes:
            for i, (template, baselines) in enumerate(probes, 1):
                top = track(template, ink)
                origin = baselines[key]
                row[f"survivor_{i}_top_px"] = top
                row[f"survivor_{i}_dy_px"] = top - origin if top is not None and origin is not None else None
        if flow == "mixed":
            top = 3 * ROW
            for label, part in zip(("sender", "subject"), LINES):
                row[f"cursor_{label}_strength"] = round(match_at(region(settled, top, part), region(ink, top, part)), 3)
        rows.append(row)
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("video", type=Path)
    parser.add_argument("flow", choices=["reversal", "mixed", "unsafe"])
    parser.add_argument("pre_key_frames", type=int, nargs="+")
    args = parser.parse_args()
    rows = analyze(args.video, args.flow, args.pre_key_frames)
    with args.video.with_suffix(".csv").open("w") as output:
        writer = csv.DictWriter(output, fieldnames=rows[0], lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)


if __name__ == "__main__":
    main()
