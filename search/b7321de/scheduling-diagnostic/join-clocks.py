"""Join the diagnostic clock snapshots to the FrameLog rows."""
import csv
import json
from pathlib import Path
import sys

rows = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines()]
clocks = [tuple(map(int, line.split())) for line in Path(sys.argv[2]).read_text().splitlines()]
queries = [(row["t_ms"], row["t_ms"] + row["ms"])
           for row in rows if row.get("event") == "search_work"]
offset = 0
out = csv.writer(sys.stdout)
out.writerow(["key", "key_to_paint_ms", "frame_wall_ms", "frame_cpu_ms",
              "probe_span_ms", "voluntary_switches", "involuntary_switches"])
for row in rows:
    if "cpu_ms" not in row:
        continue
    start, stop = clocks[offset:offset + 2]
    offset += 3 if "frame_ms" in row else 2
    measured_cpu = (stop[1] - start[1]) / 1e6
    assert abs(measured_cpu - row["cpu_ms"]) <= 0.00051, (offset, measured_cpu, row)
    if (row.get("key_ms") or 0) <= 8:
        continue
    key_start = row["t_ms"] + row["frame_ms"] - row["key_ms"]
    if any(a <= key_start <= b for a, b in queries):
        out.writerow([row["key"], row["key_ms"], row["frame_ms"], row["cpu_ms"],
                      round((stop[0] - start[0]) / 1e6, 3), stop[2] - start[2],
                      stop[3] - start[3]])
assert offset == len(clocks), (offset, len(clocks))
