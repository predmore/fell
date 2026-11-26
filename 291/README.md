# Issue 291 · Batch inbox motion

The recordings use Fell's demo mailbox inside loopback-only headless Sway. The batch flow uses `V`, nine `j` presses, and `e`. The single-row comparison uses `e` on the first thread. The baseline is `08b0f4f1` and the updated build is `9ee89a02`.

| Capture | Highest sampled survivor offset after archive | Archive frames with competing ink | Fading selection in first changed frame |
| --- | ---: | ---: | ---: |
| [Tokyo Night, baseline](dark-before.mp4) | 247 px | 0/11 | 249 px |
| [Tokyo Night](dark-after.mp4) | 0 px | 0/13 | 680 px |
| [Catppuccin Latte](light-after.mp4) | 0 px | 0/9 | 680 px |
| [Tokyo Night, 490 px](narrow-after.mp4) | 0 px | 0/16 | 680 px |
| [Single row, baseline](single-before.mp4) | 23 px | 0/5 | 23 px |
| [Single row](single-after.mp4) | 23 px | 0/7 | 23 px |
| [Reduced motion](reduced-after.mp4) | 0 px | 0/1 | 0 px |

The ten contiguous departures shift the survivors' logical slots by 680 px. On the updated build each survivor exceeds the 204 px slide threshold and fades in at its destination. Reduced motion settles on the first changed frame. The single-row flow has a 68 px slide on both builds. The software compositor can skip early samples, so the table gives the largest offset actually visible after the archive.

[measure.py](measure.py) uses the decoded video frames to match survivor glyphs against their settled positions. At each list y it checks the text column from x=16 through x=227 for strokes from both the outgoing and the arriving rows. An overlapping scanline needs at least five independent stroke pixels from each reference plus a better fit for two glyph patterns. The script verifies that this rule detects a synthetic mix of the two reference frames. The per-frame CSVs include the last still frame before archive for comparison, followed by the decoded frames through settle. The selection column at x=10 measures partial fill against its full-strength and background values.

Run the analysis with Python, NumPy and FFmpeg. For example, `python3 measure.py dark-after.mp4 10`.

## Round 2

The updated build is `433c3afa`. The mixed flow marks rows 0, 2, 4 and 6 with `x` and archives with `e`. The bottom flow uses `G`, `V k k k`, and `e`. The [ten-row dark screenshot](dark-after.png) was retaken 1.25 seconds after archive.

| Layout | Mixed capture | Mixed offsets on first changed frame | Bottom capture | Shared bottom offset on first changed frame |
| --- | --- | ---: | --- | ---: |
| Tokyo Night | [MP4](dark-mixed.mp4) · [PNG](dark-mixed.png) · [CSV](dark-mixed.csv) | +24, +48, +73 px | [MP4](dark-bottom.mp4) · [PNG](dark-bottom.png) · [CSV](dark-bottom.csv) | −74 px |
| Catppuccin Latte | [MP4](light-mixed.mp4) · [PNG](light-mixed.png) · [CSV](light-mixed.csv) | +25, +49, +74 px | [MP4](light-bottom.mp4) · [PNG](light-bottom.png) · [CSV](light-bottom.csv) | −89 px |
| Tokyo Night, 490 px | [MP4](narrow-mixed.mp4) · [PNG](narrow-mixed.png) · [CSV](narrow-mixed.csv) | +25, +50, +75 px | [MP4](narrow-bottom.mp4) · [PNG](narrow-bottom.png) · [CSV](narrow-bottom.csv) | −76 px |

[measure-round2.py](measure-round2.py) matches each moving survivor from old rows 1, 3 and 5 in the mixed batch and the last four surviving rows in the bottom batch. Positive offsets start below the destination, and negative offsets start above it. The four bottom tracks agree on every frame. All tracked glyphs stay intact throughout the observed motion. Across 114 decoded frames from the first change through settle, the script detected zero missing tracked scanlines and zero scanlines with competing ink in the sampled list column. Positive controls verify that the detector flags clipped glyphs and blended text.

The recordings preserve lossless frames from headless Sway. The software compositor repeats some frames, and the first sampled offset can be smaller than the animation's 204 px limit. The deterministic tests sample animation time from 0 to 120 ms, including the full starting offset. To regenerate a CSV, run `python3 measure-round2.py dark-mixed.mp4 mixed` with NumPy and FFmpeg.

## Round 3

Build `bf923f24` ran the demo inbox with `FELL_DEMO_EXTRA=24` inside headless Sway, with a private session bus and loopback-only networking. The recordings use the production clock. The second table comes from the same source with the motion clock slowed tenfold for frame inspection. Software rendering and capture can repeat frames or skip the first rendered pose, so the video timestamps describe the capture.

| Layout | Bottom 4 | Bottom 10 | Mixed marks | Arrivals from above |
| --- | --- | --- | --- | --- |
| Tokyo Night | [MP4](round-3/real/dark-bottom-4.mp4) · [CSV](round-3/real/dark-bottom-4.csv) | [MP4](round-3/real/dark-bottom-10.mp4) · [CSV](round-3/real/dark-bottom-10.csv) | [MP4](round-3/real/dark-mixed.mp4) · [CSV](round-3/real/dark-mixed.csv) | [MP4](round-3/real/dark-above.mp4) · [CSV](round-3/real/dark-above.csv) |
| Catppuccin Latte | [MP4](round-3/real/light-bottom-4.mp4) · [CSV](round-3/real/light-bottom-4.csv) | [MP4](round-3/real/light-bottom-10.mp4) · [CSV](round-3/real/light-bottom-10.csv) | [MP4](round-3/real/light-mixed.mp4) · [CSV](round-3/real/light-mixed.csv) | [MP4](round-3/real/light-above.mp4) · [CSV](round-3/real/light-above.csv) |
| Tokyo Night, 490 px | [MP4](round-3/real/narrow-bottom-4.mp4) · [CSV](round-3/real/narrow-bottom-4.csv) | [MP4](round-3/real/narrow-bottom-10.mp4) · [CSV](round-3/real/narrow-bottom-10.csv) | [MP4](round-3/real/narrow-mixed.mp4) · [CSV](round-3/real/narrow-mixed.csv) | [MP4](round-3/real/narrow-above.mp4) · [CSV](round-3/real/narrow-above.csv) |

Settled mixed batch screenshots in [Tokyo Night](round-3/real/dark-mixed.png), [Catppuccin Latte](round-3/real/light-mixed.png), and [Tokyo Night at 490 px](round-3/real/narrow-mixed.png).

| Slowed dark capture | Keys | First sampled displacement from pre-key pose | Total displacement |
| --- | --- | --- | --- |
| Bottom 4 · [MP4](round-3/slow/dark-bottom-4.mp4) · [CSV](round-3/slow/dark-bottom-4.csv) | `G V k k k e` | +17, +17, +17, +17 px | +272, +272, +272, +272 px |
| Bottom 10 · [MP4](round-3/slow/dark-bottom-10.mp4) · [CSV](round-3/slow/dark-bottom-10.csv) | `G V`, nine `k` presses, then `e` | +63, +63 px | +680, +680 px |
| Mixed marks · [MP4](round-3/slow/dark-mixed.mp4) · [CSV](round-3/slow/dark-mixed.csv) | `x j j x j j x j j x e` | -23, -46, -69 px | -68, -136, -204 px |
| Arrivals from above · [MP4](round-3/slow/dark-above.mp4) · [CSV](round-3/slow/dark-above.csv) | `G k x k k x k k x k k x e` | +25, +25, +25, +25, +19, +12, +6, 0 px | +272, +272, +272, +272, +204, +136, +68, 0 px |

Positive displacement is downward. The sampled bottom survivors kept their spacing throughout the recordings. Above the alternating bottom marks, the first four measured rows traveled together by 272 px. The following survivors traveled 204, 136, 68, and 0 px in order. The deterministic test checks the exact first rendered pose and every millisecond of the quadratic curve for displacements within one viewport.

In the three production mixed recordings and the slowed dark recording, the two arrivals at list y=204 and y=272 had no matched sender or subject ink in the 183 sampled slot-and-frame overlaps with a moving row. Each arrival’s two lines became visible together after clearance. The [mixed sequence](round-3/mixed-motion-review.png) shows the pre-key pose, covered arrival slots, and both reveals.

[measure-round3.py](round-3/measure-round3.py) matches opaque survivor glyphs against the settled video and estimates the sender and subject ink separately for stationary rows. Each CSV starts with the video frame matching the pre-key screenshot. Subsequent rows include displacement from that pose, offset to the final slot, template match strength, and matched ink relative to the departure or arrival reference. A first paint identical to the pre-key pose can share that reference. Run the script with NumPy, FFmpeg, and FFprobe, passing the MP4, the first frame number in its CSV, and one of `bottom-4`, `bottom-10`, `mixed`, or `above`. The original pre-key PNG can be passed in place of the frame number.
