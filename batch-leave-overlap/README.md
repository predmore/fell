# Batch archive frame measurements

The before clips are from `main` at `661c0728`. The after clips are from `c17a8746`. Each run used `fell --demo` with 32 extra fixture threads. From the top of the inbox, the key sequence was `V j j j e`.

| Theme | Build | Decoded frames | Frames during slide | Frames with competing list text |
|---|---|---:|---:|---:|
| Tokyo Night | Before | 228 | 28 | 21 |
| Tokyo Night | After | 235 | 27 | 0 |
| Catppuccin Latte | Before | 242 | 35 | 27 |
| Catppuccin Latte | After | 239 | 37 | 0 |

The script decodes each MP4 to an even-width grayscale crop of the list. It measures the survivor offset by matching three rows against their settled text. A text-free column supplies the background at each y, including the selection band. Where a sliding row covers a y, the script checks for unique text pixels from one of the four departed rows or from a bottom row at its final position. The CSVs give the frame index, measured offset, and zero-based slots where competing text was detected. The software-rendered recordings repeat presentation frames, so the frame counts describe the clips.

In both after clips the last full row was still below the viewport at the 100 px offset sample. Its first measured appearance matched the list's 22 px slide offset, then it settled with the other rows. On main, the matching frames already show bottom rows fading in at their final slots while other rows still occupy those positions.

The wide screenshots show the matching 100 px offset frames. The narrow screenshots show the 22 px offset in a 670 × 670 window.

Reproduce the frame tables with `python3 measure.py dark-*.mp4 light-*.mp4`.
