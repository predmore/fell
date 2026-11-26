import argparse
import statistics
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("video")
parser.add_argument("--mode", choices=("confirm", "paging"), default="confirm")
def span(value):
    start, end = map(int, value.split(":"))
    return range(start, end + 1)

parser.add_argument("--normal-ref", type=span, required=True)
parser.add_argument("--confirm-ref", type=span)
parser.add_argument("--light", action="store_true")
args = parser.parse_args()
x0, y0, width, height = 260, 500, 560, 44
cmd = [
    "ffmpeg", "-v", "error", "-threads", "2", "-filter_threads", "1",
    "-i", args.video, "-vf", f"crop={width}:{height}:{x0}:{y0},format=gray",
    "-fps_mode", "passthrough", "-f", "rawvideo", "-",
]
opposite = -1 if args.light else 1
frame_bytes = width * height
with subprocess.Popen(cmd, stdout=subprocess.PIPE) as proc:
    frames = list(iter(lambda: proc.stdout.read(frame_bytes), b""))
    if proc.wait():
        raise SystemExit("ffmpeg failed")
assert frames and all(len(f) == frame_bytes for f in frames)

def region(left, top, right, bottom):
    return [(y - y0) * width + x - x0 for y in range(top, bottom) for x in range(left, right)]

def bg(frame):
    return statistics.median(frame[i] for i in region(300, 503, 390, 507))

if args.mode == "confirm":
    measurements = {
        "undo_slot": region(550, 510, 618, 511),
        "confirm_title": region(280, 511, 512, 523),
    }
    keyframes = {
        "undo_slot": args.normal_ref,
        "confirm_title": args.confirm_ref,
    }
else:
    measurements = {
        "older": region(739, 518, 793, 531),
        "newer": region(652, 518, 715, 531),
    }
    keyframes = {key: args.normal_ref for key in measurements}

def ink(frame, indices):
    base = bg(frame)
    return sum(max(0, opposite * (frame[i] - base)) for i in indices) / len(indices)

masks = {}
reference = {}
for key, indices in measurements.items():
    full = max((frames[n] for n in keyframes[key]), key=lambda frame: ink(frame, indices))
    base = bg(full)
    masks[key] = [i for i in indices if opposite * (full[i] - base) >= 40]
    assert masks[key], (key, base)
    reference[key] = ink(full, masks[key])

print("frame,time_s," + ",".join(measurements))
for n, frame in enumerate(frames):
    values = [ink(frame, masks[key]) / reference[key] for key in measurements]
    print(f"{n},{n / 60:.4f}," + ",".join(f"{v:.3f}" for v in values))
