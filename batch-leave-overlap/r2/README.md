# Selection fade and top-edge arrivals

The before clips use `c17a8746`. The after clips use `4c2fa702`. All use the demo mailbox in a loopback-only nested compositor.

## Selection fade

For `V j j j e`, the old selection covers y=0–272 before the action. The table compares the first changed decoded frame in each clip. The surviving row's full band and the fading part of the old band can both contribute to the normalized value.

| Theme | Before frame | After frame | Before old-region selection | After old-region selection | Fading band before | Fading band after |
|---|---:|---:|---:|---:|---:|---:|
| Tokyo Night | 163 | 168 | 26% | 43% | 0 px | 92 px |
| Catppuccin Latte | 162 | 167 | 0% | 45% | 0 px | 100 px |
| Tokyo Night, narrow | 180 | 177 | 0% | 44% | 0 px | 100 px |

The CSVs measure each decoded frame at x=8, 10 and 12 in the list. Relative selection is the distance from the background luma to the steady selection luma, normalized from 0 to 1. Values from 0.12 up to 0.88 count as partial selection. The dark and light after clips have zero frames with competing text over the slide, over 27 and 30 decoded frames respectively.

For a single `e` in the narrow window, the old clip has up to 25 px of plain background inside the top 68 px during the slide. The new clip has zero. Its first changed frame shows both bands, followed by a 6 px fading strip before the new cursor settles at the top.

## Top edge

`G V k k e` archives the last three rows. The top screenshots show the same 17 px remaining displacement, measured from the tracked rows below. In the before clip, 842 of 959 sampled pixels in the top 20 px show ink from the arriving row at its final position while the tracked rows still have 17 px to travel. In the after clip, zero of 957 do. The new top row comes into view with its neighbors.

The move picker clip uses `V j j j m`, types `archive` and presses Return. Fell's current `m` handler moves the cursor thread, so this clip shows one row leaving.

Run `python3 measure-bands.py dark-before.mp4:4:129 dark-after.mp4:4:129 light-before.mp4:4:129 light-after.mp4:4:129 single-before.mp4:1:114 single-after.mp4:1:114 narrow-before.mp4:4:129 narrow-after.mp4:4:129` for the band measurements. Run `python3 measure-top.py top-before.mp4 top-after.mp4` for the top-edge comparison.
