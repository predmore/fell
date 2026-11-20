# Live search results layout timing

The same 100,000-message synthetic mailbox and input sequence ran against release builds of `628eed87dfbeab9d844a95463c7c5534fd7843e0` and `7dbd7df48d7385d56cce7effd3034a331b732d1a`. Both used a 1200 × 780 Fell window in isolated headless Sway on an Apple M2. Sway reported the Mesa Apple M2 GLES2 renderer, and Fell used the Asahi Vulkan ICD. Each run used disposable XDG paths and a fresh copy of the mailbox, with no local embedding model installed. The mailbox SHA-256 was `585585df8226a94d01265ef6a13d801ce64ea1eac1556b6a1c9f5b78f560f613`.

The replay uses `wtype` to type `/invoice`. It then alternates Backspace and `e` 90 times with 47 ms and 120 ms gaps. Ctrl-U clears the field. The replay types `in:inbox` and alternates Backspace and `x` 60 times with 47 ms and 185 ms gaps. Both runs recorded 317 key frames. A key overlaps search when its handler starts inside a recorded `search_work` interval. FrameLog ends at CPU paint. Display presentation is outside this measurement.

| Release | Overlapping keys over 8 ms | p95 | Maximum |
| --- | ---: | ---: | ---: |
| `628eed8` | 13 / 155 | 9.454 ms | 15.974 ms |
| `7dbd7df` | 0 / 154 | 2.408 ms | 6.485 ms |

Two other keys in the revised replay took more than 8 ms outside the recorded search intervals. The before and after settled screenshots have zero differing pixels. The original layout experiment that blanked the results was discarded and is excluded from these results.
