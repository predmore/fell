# Issue 300 motion evidence

The recordings use the demo mailbox in headless Sway with a private D-Bus and loopback-only networking. The two-pane window is 1000 × 700 px and the narrow window is 490 × 700 px. Each judge build uses a throwaway clock patch that maps real elapsed time to one tenth of that time in `Clock::now` and `Clock::frame`. The clips show `V j m`, filter to Archive, press Enter, then press `u`. The mid-slide run presses `u` while the selected survivor is still moving.

| Layout | Main at `e705723a` | Updated |
| --- | --- | --- |
| Tokyo Night | [MP4](dark-before.mp4) · [Frames](dark-before.png) · [Video CSV](dark-before.video.csv) · [App CSV](dark-before.app.csv) | [MP4](dark-after.mp4) · [Frames](dark-after.png) · [Video CSV](dark-after.video.csv) · [App CSV](dark-after.app.csv) |
| Catppuccin Latte | [MP4](light-before.mp4) · [Frames](light-before.png) · [Video CSV](light-before.video.csv) · [App CSV](light-before.app.csv) | [MP4](light-after.mp4) · [Frames](light-after.png) · [Video CSV](light-after.video.csv) · [App CSV](light-after.app.csv) |
| Tokyo Night, 490 px | [MP4](narrow-before.mp4) · [Frames](narrow-before.png) · [Video CSV](narrow-before.video.csv) · [App CSV](narrow-before.app.csv) | [MP4](narrow-after.mp4) · [Frames](narrow-after.png) · [Video CSV](narrow-after.video.csv) · [App CSV](narrow-after.app.csv) |
| Tokyo Night, mid-slide undo | | [MP4](dark-after-mid-slide.mp4) · [Frames](dark-after-mid-slide.png) · [Video CSV](dark-after-mid-slide.video.csv) · [App CSV](dark-after-mid-slide.app.csv) |
| Tokyo Night, undo during picker exit | | [MP4](dark-final-rapid-interrupt.mp4) · [Frames](dark-final-rapid-interrupt.png) · [Video CSV](dark-final-rapid-interrupt.video.csv) · [App CSV](dark-final-rapid-interrupt.app.csv) |

## Observations

- On main, the Tokyo Night picker is still legible after Mira moves 54 px up from its pre-Enter position (frame 316). At light frame 323 Mira has moved 73 px and the card remains visible. The 490 px run shows the two motions together at frame 318.
- At main frame 437 the Offsite sender is already visible while the subject line has yet to clear the clip. The first changed dark undo frame shows 48 px of the 68 px selection band. The light and narrow baselines show the same early jump to a shortened band.
- In the updated captures, Mira remains at y=180 while the picker fades. The first sampled moving frame with Mira displaced by more than 10 px is 395 in dark (y=118), 374 in light (y=111), and 388 at 490 px (y=94). No picker is visible in those frames.
- On settled undo, the band stays a full 68 px at the top because the restored selection occupies the same slot as the prior cursor. The restored sender and subject fade in together when each slot clears the survivor. At 490 px, undo happens during the slide and the full band travels from y=68 to y=44. The separate wide mid-slide clip shows y=77 to y=44 while the survivors reverse and the restored rows arrive whole.
- With a rapid undo during the picker exit, the restored rows stay visible and Mira remains at y=180 across Enter and undo. The selected range gives way to the restored cursor band as the picker continues fading.
- Main already preserves painted starts for survivors on an interrupted shift and stops rendering a departure ghost when that row returns. The updated undo uses the same ordered starts and whole-row arrival layers for restored rows. [#332](https://github.com/predmore/fell/pull/332)

## How to read the CSVs

The app CSV sets time zero to the app frame immediately before the frame tagged Enter or `u` in `FELL_FRAME_LOG`. The video CSV uses the last recording frame before the visible response as its corresponding reference. Video indices are zero-based at 60 frames per second and can repeat a compositor paint. Software rendering affects pacing. Use these recordings to compare what paints together and how positions change.

The picker column measures prompt edge contrast relative to the opaque pre-Enter prompt. Interpret it during the picker exit, since other text can later cross the sampled area. Row tops are measured from the window edge by matching Mira's first line. A blank means the match was too weak. Selection spans are inclusive pixel ranges sampled in the left padding. The four ink columns track matched edge contrast relative to the final frame, in the two restored sender and subject lines. Small negative or above-one values can come from overlapping text and video quantization.
