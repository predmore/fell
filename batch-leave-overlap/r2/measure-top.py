"""Measure displacement below the clip and look for early ink in the top strip."""
import csv
from pathlib import Path
from statistics import median
import subprocess
import sys

WIDTH = 232
HEIGHT = 362


def frames(path):
    command = [
        "ffmpeg", "-v", "error", "-i", path, "-vf",
        f"crop={WIDTH}:{HEIGHT}:4:44,format=gray", "-fps_mode", "passthrough",
        "-f", "rawvideo", "-",
    ]
    with subprocess.Popen(command, stdout=subprocess.PIPE) as reader:
        while pixels := reader.stdout.read(WIDTH * HEIGHT):
            assert len(pixels) == WIDTH * HEIGHT
            yield pixels
        assert reader.wait() == 0


def measure(path):
    decoded = list(frames(path))
    final = decoded[-1]
    background = median(final[6::WIDTH])
    ink = [
        y*WIDTH+x for y in range(210, 351) for x in range(14, 165, 2)
        if final[y*WIDTH+x] >= background + 48
    ]
    blanks = [
        y*WIDTH+x for y in range(210, 351) for x in range(14, 165, 2)
        if final[y*WIDTH+x] <= background + 2 and (x+2*y) % 23 == 0
    ]
    top_ink = [
        y*WIDTH+x for y in range(0, 20) for x in range(14, WIDTH-1)
        if final[y*WIDTH+x] >= background + 48
    ]

    def match(frame, dy):
        offset = dy * WIDTH
        return (
            sum(frame[index + offset] for index in ink) / len(ink)
            - sum(frame[index + offset] for index in blanks) / len(blanks)
        )

    seen = {}
    data = []
    for index, frame in enumerate(decoded):
        if index < 236:
            continue
        if frame not in seen:
            dy = max(range(-208, 1), key=lambda shift: match(frame, shift))
            allowed_blank = [
                pixel for pixel in top_ink
                if max(final[pixel - dy*WIDTH + dist] for dist in (0, -1, 1)) <= background + 4
            ]
            fixed_ink = sum(frame[pixel] >= background + 13 for pixel in allowed_blank)
            seen[frame] = dy, round(match(frame, dy)), fixed_ink, len(allowed_blank)
        data.append((index, *seen[frame]))

    print(path.name)
    previous = None
    for row in data:
        _, dy, quality, hits, total = row
        if row[1:] != previous and quality > 40:
            print(" ", row)
        previous = row[1:]
    with path.with_suffix(".csv").open("w", newline="") as output:
        writer = csv.writer(output, lineterminator="\n")
        writer.writerow([
            "frame", "offset_of_tracked_rows", "live_text_match",
            "top_strip_ink_at_final_position", "top_strip_unique_ink_samples",
        ])
        writer.writerows(data)


if __name__ == "__main__":
    for name in sys.argv[1:]:
        measure(Path(name))
