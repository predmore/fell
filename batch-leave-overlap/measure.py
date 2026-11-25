"""Measure V-j-j-j-e demo recordings from their decoded grayscale frames."""
from collections import defaultdict
import csv
from pathlib import Path
import subprocess
import sys

ROW = 68
CROP_TOP = 44
CROP_LEFT = 4
BEFORE_REFERENCE = 110
HIGHLIGHT_X = 6
TEXT_X = range(13, 228)
CORE = 48
MIN_CONTRAST = 11
LIGHTER = [bytes(max(0, p-bg) for p in range(256)) for bg in range(256)]
DARKER = [bytes(max(0, bg-p) for p in range(256)) for bg in range(256)]


def read_clip(path):
    w, h = map(int, subprocess.check_output([
        'ffprobe', '-v', 'error', '-select_streams', 'v:0', '-show_entries',
        'stream=width,height', '-of', 'csv=p=0', path,
    ]).split(b','))
    width = 450 if w == 1200 else w - 8
    height = h - CROP_TOP - 30
    cmd = [
        'ffmpeg', '-v', 'error', '-i', path, '-vf',
        f'crop={width}:{height}:{CROP_LEFT}:{CROP_TOP},format=gray',
        '-fps_mode', 'passthrough', '-f', 'rawvideo', '-',
    ]
    frames = []
    with subprocess.Popen(cmd, stdout=subprocess.PIPE) as ffmpeg:
        while frame := ffmpeg.stdout.read(width * height):
            assert len(frame) == width * height
            frames.append(frame)
        assert ffmpeg.wait() == 0
    return frames, width, height


def contrast(frame, width, height):
    dark = frame[HIGHLIGHT_X] < 128
    # Clamp noise on the opposite side of the background. Store as bytes
    # for cheap indexing. x=10 in the original image is free of row ink.
    field = bytearray()
    for y in range(height):
        line = frame[y*width:(y+1)*width]
        bg = line[HIGHLIGHT_X]
        field.extend(line.translate((LIGHTER if dark else DARKER)[bg]))
    return field


def samples(final, width, row):
    ink, blank = [], []
    for y in range(row * ROW + 14, row * ROW + 56):
        for x in range(13, 160, 2):
            k = y*width+x
            if final[k] >= CORE:
                ink.append(k)
            elif final[k] <= 2 and (2*x+3*y) % 23 == 0:
                blank.append(k)
    return ink, blank


def score(field, anchors, width, dy):
    offset = dy * width
    return sum(
        sum(field[k+offset] for k in ink) / len(ink) -
        sum(field[k+offset] for k in blank) / len(blank)
        for ink, blank in anchors
    ) / len(anchors)


def match_shift(field, anchors, width):
    def score_at(dy):
        return score(field, anchors, width, dy)
    dy = max(range(4*ROW+1), key=score_at)
    return dy, round(score_at(dy))


def competing_ink(field, final, other, width, height, dy, preview):
    # A moving live row owns every y at/after its first covered pixel.
    # Search for the other text's unique core strokes within that area.
    found = defaultdict(lambda: [0, 0])
    start = max(dy, height-4*ROW) if preview else dy
    end = height if preview else min(4*ROW, height)
    for y in range(start, end):
        for x in TEXT_X:
            target = y*width+x
            live = target-dy*width
            if other[target] < CORE or final[live] > 2:
                continue
            # Allow a one-pixel codec/layout tolerance around live text.
            if any(final[live+d] > 4 for d in (-1, 1) if 0 <= live+d < len(final)):
                continue
            if y > dy and final[live-width] > 4:
                continue
            if live + width < len(final) and final[live+width] > 4:
                continue
            count = found[y//ROW]
            count[0] += 1
            count[1] += field[target] >= MIN_CONTRAST
    return {
        row: (hits, total) for row, (total, hits) in sorted(found.items())
        if hits >= max(12, total * 0.10)
    }


def measure(path):
    frames, width, height = read_clip(path)
    before = contrast(frames[BEFORE_REFERENCE], width, height)
    final = contrast(frames[-1], width, height)
    anchors = [samples(final, width, i) for i in (0, 1, 2)]
    # Last full reference row, among those previously below the viewport.
    tail_row = height // ROW - 1
    tail_anchor = [samples(final, width, tail_row)]
    records = []
    arrival = False
    overlaps = 0
    transient = 0
    last = None
    unique = {}
    for index, raw in enumerate(frames):
        if index < 120:
            continue
        fingerprint = hash(raw)
        if fingerprint not in unique:
            field = contrast(raw, width, height)
            dy, live_match = match_shift(field, anchors, width)
            ghost = competing_ink(field, final, before, width, height, dy, False)
            preview = competing_ink(field, final, final, width, height, dy, True)
            tail_start = tail_row * ROW + dy
            tail_contrast = ''
            if tail_start + 56 < height:
                tail_contrast = round(score(field, tail_anchor, width, dy))
            unique[fingerprint] = dy, live_match, ghost, preview, tail_contrast
        dy, live_match, ghost, preview, tail_contrast = unique[fingerprint]
        if dy == 272 and not preview and not arrival:
            continue
        if live_match < 20:
            print(f'  Frame {index} unmatched ({dy=}, {live_match=})')
            continue
        arrival = True
        if dy == 0:
            preview = {}
        records.append([index, dy, live_match, ';'.join(map(str, ghost)),
                        ';'.join(map(str, preview)), tail_contrast])
        transient += int(dy != 0 or bool(ghost))
        overlaps += int(bool(ghost or preview))
        # Avoid printing repeated output when the recorder duplicates frames.
        key = (dy, tuple(ghost), tuple(preview))
        if key != last:
            last = key
            print(f'  {index:3d} dy={dy:3d} match={live_match:3d} ghost={ghost} bottom={preview}')
    print(f'  {len(frames)} decoded frames, {transient} during the slide, {overlaps} with competing ink')
    assert transient > 0, 'no archive motion found'
    assert last == (0, (), ()), 'recording did not settle'
    with path.with_suffix('.csv').open('w', newline='') as output:
        csv.writer(output, lineterminator='\n').writerows([
            ['frame', 'slide_offset_px', 'live_text_match', 'ghost_overlap_slots',
             'bottom_preview_overlap_slots', 'bottom_row_match_at_slide_offset'],
            *records,
        ])


for name in sys.argv[1:]:
    path = Path(name)
    print(path.name, flush=True)
    measure(path)
