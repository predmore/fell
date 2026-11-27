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

## Round 4

Build `d11d62a2` ran the demo inbox with 24 extra fixture threads in headless Sway, on a private D-Bus with loopback-only networking. The recordings use the production motion clock, and the contact sheets use the decoded video frames at the listed timestamps.

| Layout | Top ten | Mixed marks | Bottom ten | Archive and undo | Contact sheet |
| --- | --- | --- | --- | --- | --- |
| Tokyo Night, 1200 px | [MP4](round-4/dark-top-10.mp4) · [CSV](round-4/dark-top-10.csv) | [MP4](round-4/dark-mixed.mp4) · [CSV](round-4/dark-mixed.csv) | [MP4](round-4/dark-bottom-10.mp4) · [CSV](round-4/dark-bottom-10.csv) | [MP4](round-4/dark-interrupt.mp4) · [CSV](round-4/dark-interrupt.csv) | [PNG](round-4/dark-sequence.png) |
| Catppuccin Latte, 1200 px | [MP4](round-4/light-top-10.mp4) · [CSV](round-4/light-top-10.csv) | [MP4](round-4/light-mixed.mp4) · [CSV](round-4/light-mixed.csv) | [MP4](round-4/light-bottom-10.mp4) · [CSV](round-4/light-bottom-10.csv) | [MP4](round-4/light-interrupt.mp4) · [CSV](round-4/light-interrupt.csv) | [PNG](round-4/light-sequence.png) |
| Tokyo Night, 490 px | [MP4](round-4/narrow-top-10.mp4) · [CSV](round-4/narrow-top-10.csv) | [MP4](round-4/narrow-mixed.mp4) · [CSV](round-4/narrow-mixed.csv) | [MP4](round-4/narrow-bottom-10.mp4) · [CSV](round-4/narrow-bottom-10.csv) | [MP4](round-4/narrow-interrupt.mp4) · [CSV](round-4/narrow-interrupt.csv) | [PNG](round-4/narrow-sequence.png) |

| Measurement | Tokyo Night | Catppuccin Latte | Tokyo Night, 490 px |
| --- | ---: | ---: | ---: |
| Top-ten frames with an empty line or competing lines | 0 / 20 | 0 / 17 | 0 / 10 |
| Mixed cursor row's remaining overlap with the preceding survivor when both lines first match | 1 px | 1 px | 1 px |
| Bottom-ten first sampled displacement of both survivors | +258 px | +210 px | +153 px |
| Bottom-ten final displacement of both survivors | +680 px | +680 px | +680 px |
| Furthest sampled displacement before the slide reverses after undo | +583 px | +562 px | +598 px |

The top batch uses `V`, nine `j` presses, then `e`. All ten slots show only replacement text in the first changed frame at about 35–37% of settled glyph contrast. The deterministic test checks that the replacement starts at 0.4 opacity as the ghost crosses 0.5. The mixed batch marks old slots 0, 2, 4 and 6 with `x` and uses `e`. Both cursor lines show while the previous survivor still overlaps the slot's top padding.

The bottom batch uses `G`, `V`, nine `k` presses, then `e`. The two measured survivors keep their 68 px separation on every matched frame, including during the undo reversal. The quadratic 160 ms curve moves the group about 134 px in a nominal 60 Hz step, compared with 176 px on the 120 ms curve. The software compositor can repeat frames or skip early poses, so the first sampled displacements and timestamps describe the video. The undo flow sends `u` during the slide.

[measure-round4.py](round-4/measure-round4.py) limits measurement to the list text column. It uses the last frame before the list changes for displacement and departure references, and the settled frame for arrival references. Horizontal glyph edges avoid counting the moving selection fill as text. The top-ten count flags a line as empty if both reference strengths are below 12%, and as competing if both exceed 12%. The displacement columns are blank when the tracked glyph match falls below the confidence threshold. Regenerate a CSV using NumPy and FFmpeg, for example `python3 measure-round4.py dark-top-10.mp4 53 top-10`. Use the first frame number from the corresponding CSV.

## Round 5

The release captures use the Round 5 changes in `18727ad9`, the demo inbox with 24 extra fixture threads, and headless Sway with a private D-Bus and loopback-only networking. The production build uses Fell's normal motion clock. The second build advances the same clock at one-tenth speed.

- **Bottom reversal.** Use `G`, `V`, nine `k` presses, then `e` and `u` 60 ms apart. The slowed capture uses a 600 ms delay.
- **Unsafe ordering.** Use `G` and mark four alternating rows with `x k k x k k x k k x`. Send `e`, `u`, and `e` at intervals of 60 ms and 18 ms in the production capture, or 600 ms and 180 ms in the slowed capture.
- **Mixed marks.** Mark slots 0, 2, 4 and 6 using `x j j x j j x j j x`, then press `e`.

| Layout and clock | Bottom reversal | Unsafe ordering | Mixed marks |
| --- | --- | --- | --- |
| Tokyo Night, 1200 px · production | [MP4](round-5/real/dark-reversal.mp4) · [CSV](round-5/real/dark-reversal.csv) | [MP4](round-5/real/dark-unsafe.mp4) · [CSV](round-5/real/dark-unsafe.csv) | [MP4](round-5/real/dark-mixed.mp4) · [CSV](round-5/real/dark-mixed.csv) |
| Catppuccin Latte, 1200 px · production | [MP4](round-5/real/light-reversal.mp4) · [CSV](round-5/real/light-reversal.csv) | [MP4](round-5/real/light-unsafe.mp4) · [CSV](round-5/real/light-unsafe.csv) | [MP4](round-5/real/light-mixed.mp4) · [CSV](round-5/real/light-mixed.csv) |
| Tokyo Night, 490 px · production | [MP4](round-5/real/narrow-reversal.mp4) · [CSV](round-5/real/narrow-reversal.csv) | [MP4](round-5/real/narrow-unsafe.mp4) · [CSV](round-5/real/narrow-unsafe.csv) | [MP4](round-5/real/narrow-mixed.mp4) · [CSV](round-5/real/narrow-mixed.csv) |
| Tokyo Night, 1200 px · slowed | [MP4](round-5/slow/dark-reversal.mp4) · [CSV](round-5/slow/dark-reversal.csv) | [MP4](round-5/slow/dark-unsafe.mp4) · [CSV](round-5/slow/dark-unsafe.csv) | [MP4](round-5/slow/dark-mixed.mp4) · [CSV](round-5/slow/dark-mixed.csv) |
| Catppuccin Latte, 1200 px · slowed | [MP4](round-5/slow/light-reversal.mp4) · [CSV](round-5/slow/light-reversal.csv) | [MP4](round-5/slow/light-unsafe.mp4) · [CSV](round-5/slow/light-unsafe.csv) | [MP4](round-5/slow/light-mixed.mp4) · [CSV](round-5/slow/light-mixed.csv) |
| Tokyo Night, 490 px · slowed | [MP4](round-5/slow/narrow-reversal.mp4) · [CSV](round-5/slow/narrow-reversal.csv) | [MP4](round-5/slow/narrow-unsafe.mp4) · [CSV](round-5/slow/narrow-unsafe.csv) | [MP4](round-5/slow/narrow-mixed.mp4) · [CSV](round-5/slow/narrow-mixed.csv) |

Slowed contact sheets for [Tokyo Night](round-5/dark-slow-sequence.png), [Catppuccin Latte](round-5/light-slow-sequence.png), and [Tokyo Night at 490 px](round-5/narrow-slow-sequence.png).

| Bottom reversal | Survivor tops at handoff (px) | Largest between-row blank gap after handoff | Settled blank gap |
| --- | --- | ---: | ---: |
| Tokyo Night, 1200 px · production | 472, 540 on frames 66 and 76 | 32 px | 32 px |
| Catppuccin Latte, 1200 px · production | 314, 382 on frame 52 | 32 px | 32 px |
| Tokyo Night, 490 px · production | 77, 145 on frames 66 and 70 | 32 px | 32 px |
| Tokyo Night, 1200 px · slowed | 168, 236 on frames 75 and 80 | 32 px | 32 px |
| Catppuccin Latte, 1200 px · slowed | 112, 180 on frames 123 and 137 | 32 px | 32 px |
| Tokyo Night, 490 px · slowed | 324, 392 on frames 85 and 87 | 32 px | 32 px |

The final production release also replayed the Tokyo Night reversal. The largest between-row blank gap from the handoff through settle measured 32 px. [MP4](round-5/real/final-head-dark-reversal.mp4) · [CSV](round-5/real/final-head-dark-reversal.csv)

At the bottom reversal, the deterministic test samples every millisecond through the 160 ms curve. It checks the first frame against the painted tops, then checks the distance between consecutive painted rows against the 68 px row height. The returning tail starts at 40% opacity and fades in over 120 ms. The [Round 4 reversal clips](#round-4) preserve the preceding implementation for comparison.

The [unsafe-order trace](round-5/unsafe-order.csv) lists pairs from the instrumented slowed release build. A hidden arrival appears before an opaque slider in the new order while the slider's text covers its slot. The deterministic test isolates the same case at indices 0 and 1 and checks that the slider keeps its painted position, the arrival stays hidden during overlap, and competing ghost ink is cleared.

For the mixed archive, the two text lines appear together as soon as the neighboring text box clears the cursor slot. Padding can still overlap when the fade starts. The deterministic coverage test checks the 80 ms fade from clearance.

[measure-round5.py](round-5/measure-round5.py) reads the lossless captures with FFmpeg and NumPy. Each key has its own time origin on the video frame immediately preceding the request, aligned with the screencopy clock. Blank gap is the longest vertical run between occupied text scanlines in the list column. The CSV records trailing space below the last text separately. The bottom table measures from the last changed frame at the furthest matched survivor position through settle. Both measured rows come from the pre-archive frame. The mixed CSV uses the settled sender and subject as references and reports their contrast separately. Software rendering can repeat frames or delay the next list paint. To reproduce a CSV, pass the MP4, the flow name, and the frame numbers marked `pre_key` in that CSV.

## Round 6

Build `78ece945` ran the demo inbox at 1200 × 780 in Tokyo Night with headless Sway, a private D-Bus, and loopback-only networking. Both recordings use a capture build whose motion clock advances at one-tenth speed. The mixed recording uses the simple reader setting.

| Flow | Capture | Measurements |
| --- | --- | --- |
| Archive the top four with `V j j j e`, then press `u` | [MP4](round-6/top-four-undo.mp4) · [Frames](round-6/top-four-undo.png) | [CSV](round-6/top-four-undo.csv) |
| Mark rows 0, 2, 4 and 6 with `x`, then press `e` | [MP4](round-6/mixed.mp4) · [Frames](round-6/mixed.png) | [CSV](round-6/mixed.csv) |

- **Top-four undo.** The handoff used a painted frame 102.3 ms into the archive shift. The two tracked survivors were at list y=0 and y=68 px before undo (frame 42). Both stayed at those positions on the first changed frame (47) with 100% of their original glyph contrast. On frame 48 they had both moved down 20 px. Both settled 272 px below their pre-undo positions.
- **Mixed archive.** The three sliders settled 68, 136 and 204 px above their pre-archive positions. On frame 41 the last slider's background still overlapped the cursor arrival's text area by 1 px. Neither line had matched ink. On frame 42 the background had cleared by 4 px, and both lines appeared at about 44% of settled glyph contrast.

Each displacement uses the frame captured immediately before its key. Positive displacement is downward. The CSVs include sample timestamps relative to that frame in wall-clock units and at one-tenth scale. Software rendering can repeat poses or skip a paint between samples.
