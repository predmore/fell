# Activity footer measurement

The recordings use the demo inbox in headless Sway. The fixture added 50 historical actions and a bulk archive of 12 demo conversations. The flow opens Activity, opens and cancels the undo prompt, pages older twice and newer twice, and confirms the bulk undo. Video frame numbers start at zero. The compositor can repeat a presented frame over several video frames.

## Confirm open and cancel

The script samples the top stroke of the slot-1 button and the ink of the confirmation title in disjoint regions. It builds each mask from a settled reference and reports ink strength relative to that reference. A recorded frame counts as an overlap when both regions exceed 3% strength.

| Recording | Frames inspected | Open overlap | Cancel overlap |
| --- | ---: | ---: | ---: |
| Baseline | 150–329 | 202–210 | 270–278 |
| Updated | 240–564 | 0 frames | 0 frames |

The updated recording first shows the confirmation title at frame 305 with the Undo slot already clear. Undo first returns at frame 494 with the title clear. The Rust regression test also samples the handoff at 1 ms intervals.

## Reduced-motion paging

Across frames 700–1099, both paging controls are available while the list changes twice. The Newer label reads 99.8–100% of its settled ink strength and Older reads 100%. The Rust test exercises both the pending and delayed-disabled states.

## Reproduce the frame reads

```sh
python3 measure.py before-confirm-flow.mp4 --normal-ref 75:160 --confirm-ref 215:265
python3 measure.py confirm-flow.mp4 --normal-ref 150:240 --confirm-ref 360:445
python3 measure.py reduced-paging-flow.mp4 --mode paging --light --normal-ref 710:770
```
