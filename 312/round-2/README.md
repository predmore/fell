# All inboxes round 2

Captured from `43e24f18a770b9b0b16ab1cb03160f85ca495ca2` in nested headless Sway with two synthetic IMAP accounts. Tokyo Night and Catppuccin Latte use 1200 × 780 windows. The narrow window is 490 × 780. Only Studio has Clients and only Personal has Receipts.

## First entry

Each CSV starts on the last captured frame before the virtual keyboard sent `I` for `Space I` and ends on the first frame with settled header contrast. Times use the screencopy timestamp relative to the key request. The script checked for visible content in the list and in the split reader on every frame. The contact strips show the frame before the key, the last outgoing frame, and the incoming fade. Timing reflects the software compositor and capture workload.

| Profile | Frames checked | Blank list frames | Blank reader frames | Frame-by-frame data | Contact strip |
| --- | ---: | ---: | --- | --- | --- |
| Tokyo Night | 51 | 0 | 0 | [CSV](dark-entry-frames.csv) | [PNG](dark-entry-strip.png) |
| Catppuccin Latte | 54 | 0 | 0 | [CSV](light-entry-frames.csv) | [PNG](light-entry-strip.png) |
| 490 px | 52 | 0 | Reader hidden in list mode | [CSV](narrow-entry-frames.csv) | [PNG](narrow-entry-strip.png) |

## Keyboard flow

The recording opens All inboxes with `Space I`, opens a Studio thread, then marks three conversations across both accounts. The Move picker retains Clients and Receipts as unavailable choices with their owning accounts. Filtering for `lient` leaves the Clients explanation visible. Enter leaves the picker open, Escape closes it, `e` archives the selection, and `u` restores it with “Moved back.”

[Tokyo Night](dark-keyboard-flow.mp4) · [Catppuccin Latte](light-keyboard-flow.mp4) · [490 px](narrow-keyboard-flow.mp4)
