"""Summarize key-to-paint and frame work for an exact-build FrameLog JSONL."""
import json
import math
import sys
from collections import Counter
from pathlib import Path


def nearest_rank(values, fraction):
    ordered = sorted(values)
    return None if not ordered else ordered[math.ceil(fraction * len(ordered)) - 1]


def summary(label, values):
    if not values:
        print(f"  {label}: no samples")
        return
    p50, p95, maximum = (nearest_rank(values, p) for p in (0.50, 0.95, 1.0))
    extra = f", {sum(v > 8 for v in values)}/{len(values)} over 8 ms" if label.startswith("Key handler") else ""
    print(f"  {label}: p50 {p50:.3f} ms, p95 {p95:.3f} ms, max {maximum:.3f} ms{extra}")


for file in sys.argv[1:]:
    rows = [json.loads(line) for line in Path(file).read_text().splitlines()]
    queries = [
        (row["t_ms"], row["t_ms"] + row["ms"])
        for row in rows
        if row.get("event") == "search_work"
    ]
    keys = [row for row in rows if row.get("key_ms") is not None]
    boundary = next((row["t_ms"] for row in keys if row["key"] == "ctrl-u"), float("inf"))

    def key_start(row):
        return row["t_ms"] + row["frame_ms"] - row["key_ms"]

    overlapped = [
        row for row in keys
        if any(start <= key_start(row) <= end for start, end in queries)
    ]
    print(file)
    print(f"  {len(queries)} completed work intervals, {len(keys)} key frames")
    for label, subset in (
        ("all keys", keys),
        ("keys during query work", overlapped),
        ("keys during query work in the keyword phase",
         [row for row in overlapped if key_start(row) < boundary]),
        ("keys during query work in the filter phase",
         [row for row in overlapped if key_start(row) >= boundary]),
    ):
        print(f" {label} (n={len(subset)})")
        summary("Key handler to end of paint", [row["key_ms"] for row in subset])
        summary("Frame build/layout/paint wall time", [row["frame_ms"] for row in subset])
        summary("Wait before render", [max(0, row["key_ms"] - row["frame_ms"]) for row in subset])
        summary("Frame UI-thread CPU", [row["cpu_ms"] for row in subset])
        print("  Keys", dict(Counter(row["key"] for row in subset)))
