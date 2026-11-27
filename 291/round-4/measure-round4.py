"""Measure list motion from lossless headless Sway captures with NumPy and FFmpeg."""

import argparse
import csv
from pathlib import Path
import subprocess

import numpy as np


ROW = 68
HEIGHT = 826
WIDTH = 345
LEFT = 16
TOP = 44
LINE_REGIONS = (slice(12, 35), slice(35, 56))


def decode(path):
    data = subprocess.check_output([
        "ffmpeg", "-nostdin", "-v", "error", "-threads", "2", "-i", str(path),
        "-vf", f"format=gray,crop={WIDTH}:{HEIGHT}:{LEFT}:{TOP}",
        "-threads", "1", "-pix_fmt", "gray", "-f", "rawvideo", "-",
    ])
    return np.frombuffer(data, np.uint8).reshape(-1, HEIGHT, WIDTH)


def glyphs(frame):
    return np.diff(frame.astype(np.float32), axis=-1)


def lines(frame, slot):
    return [frame[slot * ROW + part.start:slot * ROW + part.stop] for part in LINE_REGIONS]


def gain(reference, sample):
    return float(np.vdot(reference, sample) / np.vdot(reference, reference))


def pair_gains(outgoing, incoming, sample):
    template = np.column_stack([outgoing.ravel(), incoming.ravel()])
    return np.linalg.lstsq(template, sample.ravel(), rcond=None)[0]


def track(template, frame):
    # Distinguish similarly named filler rows by their subject, including its number.
    height = template.shape[0]
    windows = np.lib.stride_tricks.sliding_window_view(frame, height, axis=0)
    y, x = np.nonzero(np.abs(template) > 8)
    candidates = windows[:, x, y]
    ink = template[y, x]
    dot = candidates @ ink
    scores = dot / np.maximum(1.0, np.linalg.norm(candidates, axis=1) * np.linalg.norm(ink))
    best = int(np.argmax(scores))
    return best, float(scores[best])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("video", type=Path)
    parser.add_argument("pre_key", help="Pre-key PNG or the first frame number in the published CSV")
    parser.add_argument("flow", choices=["top-10", "mixed", "bottom-10", "interrupt"])
    args = parser.parse_args()

    frames = decode(args.video)
    if args.pre_key.isdecimal():
        baseline = int(args.pre_key)
    else:
        pre_key = decode(args.pre_key)[0]
        difference = np.mean(np.abs(frames.astype(np.int16) - pre_key), axis=(1, 2))
        first_match = int(np.flatnonzero(difference < 0.5)[0])
        # Ignore sub-level changes in fill while the marked selection settles.
        baseline = first_match + int(np.flatnonzero(difference[first_match:] >= 1.0)[0]) - 1
    last_change = int(np.flatnonzero(np.any(frames[1:] != frames[:-1], axis=(1, 2)))[-1] + 1)
    pre = glyphs(frames[baseline])
    settled = glyphs(frames[-1])
    records = []
    for index in range(baseline, last_change + 1):
        row = {"frame": index, "ms_from_pre_key_frame": round((index - baseline) * 1000 / 60, 1)}
        sample = glyphs(frames[index])
        if args.flow == "top-10":
            matches = [
                pair_gains(old, new, current)
                for slot in range(10)
                for old, new, current in zip(lines(pre, slot), lines(settled, slot), lines(sample, slot))
            ]
            row.update(
                empty_lines=sum(max(match) < 0.12 for match in matches),
                competing_lines=sum(min(match) > 0.12 for match in matches),
                weakest_visible_line=round(min(map(max, matches)), 3),
                strongest_departing_line=round(max(match[0] for match in matches), 3),
                weakest_incoming_line=round(min(match[1] for match in matches), 3),
            )
        elif args.flow == "mixed":
            text_top = 5 * ROW + 12
            found, confidence = track(pre[text_top:text_top + 44], sample)
            painted_top = found - 12
            row.update(
                previous_survivor_top=painted_top if confidence >= 0.94 else "",
                previous_survivor_match=round(confidence, 3),
            )
            row.update(zip(("cursor_sender", "cursor_subject"), [
                round(gain(reference, current), 3)
                for reference, current in zip(lines(settled, 3), lines(sample, 3))
            ]))
        else:
            for name, pre_top in (("first", HEIGHT - 12 * ROW), ("second", HEIGHT - 11 * ROW)):
                text_top = pre_top + 12
                found, confidence = track(pre[text_top:text_top + 44], sample)
                row[name + "_dy"] = found - text_top if confidence >= 0.94 else ""
                row[name + "_match"] = round(confidence, 3)
        records.append(row)

    with args.video.with_suffix(".csv").open("w") as output:
        writer = csv.DictWriter(output, fieldnames=records[0])
        writer.writeheader()
        writer.writerows(records)
    print(args.video.stem, "pre", baseline, "settled", last_change, "frames", len(records))
    if args.flow == "top-10":
        print("empty lines", sum(row["empty_lines"] for row in records),
              "competing lines", sum(row["competing_lines"] for row in records),
              "weakest visible line", min(row["weakest_visible_line"] for row in records))
    if args.flow == "mixed":
        print("first frame with both cursor lines", next(
            row for row in records if min(row["cursor_sender"], row["cursor_subject"]) >= 0.3
        ))
    if args.flow in ("bottom-10", "interrupt"):
        print("matched survivor displacement pairs", list(dict.fromkeys(
            (row["first_dy"], row["second_dy"]) for row in records
        )))


if __name__ == "__main__":
    main()
