"""Read the selection stripe across the list at x=8, 10 and 12."""
import csv
from pathlib import Path
from statistics import median
import subprocess
import sys


def frames(path):
    height = int(subprocess.check_output([
        "ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries",
        "stream=height", "-of", "csv=p=0", path,
    ])) - 74
    command = [
        "ffmpeg", "-v", "error", "-i", path, "-vf",
        f"crop=8:{height}:8:44,format=gray", "-fps_mode", "passthrough",
        "-f", "rawvideo", "-",
    ]
    with subprocess.Popen(command, stdout=subprocess.PIPE) as reader:
        while pixels := reader.stdout.read(8 * height):
            assert len(pixels) == 8 * height
            yield [median(pixels[i:i+5:2]) for i in range(0, len(pixels), 8)]
        assert reader.wait() == 0


def measure(path, selected_rows, reference_frame):
    columns = list(frames(path))
    old_height = selected_rows * 68
    settled = columns[-1]
    background = median(settled[360:600])
    highlight = median(settled[4:64])
    difference = highlight - background
    assert abs(difference) >= 8, "selection is missing in the settled frame"

    def relative(column):
        return [max(0.0, min(1.0, (gray - background) / difference)) for gray in column]

    def bands(column, threshold):
        visible = [y for y, value in enumerate(column) if value >= threshold]
        result = []
        for y in visible:
            if result and result[-1][1] == y:
                result[-1][1] += 1
            else:
                result.append([y, y+1])
        return " ".join(f"{start}-{end}" for start, end in result)

    initial = relative(columns[reference_frame])
    assert sum(initial[:old_height]) / old_height > 0.95, "old marks must be visible"
    data = []
    for index, column in enumerate(columns):
        normalized = relative(column)
        old = normalized[:old_height]
        data.append([
            index, round(sum(old) / old_height, 3),
            sum(value < 0.12 for value in old),
            sum(0.12 <= value < 0.88 for value in old),
            bands(normalized, 0.12), bands(normalized, 0.88),
        ])

    archive = next(
        index for index in range(reference_frame + 1, len(columns))
        if sum(abs(a-b) >= 0.22 for a, b in zip(initial, relative(columns[index]))) >= 8
    )
    print(f"{path.name}: bg={background} selected={highlight}, transition starts at frame {archive}")
    with path.with_suffix(".csv").open("w", newline="") as output:
        writer = csv.writer(output, lineterminator="\n")
        writer.writerow([
            "frame", "old_region_mean_relative_selection", "old_region_background_px",
            "old_region_partial_selection_px", "any_selection_y", "full_selection_y",
        ])
        writer.writerows(data)
    previous = None
    for frame in data[archive - 1:archive + 70]:
        key = tuple(frame[2:])
        if key != previous:
            print(" ", frame)
        previous = key


if __name__ == "__main__":
    for argument in sys.argv[1:]:
        name, rows, reference = argument.rsplit(":", 2)
        measure(Path(name), int(rows), int(reference))
