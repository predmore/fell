# Background thread priority diagnostic

These trials used the exact 7f53378 release binary and the same fixture and input as the adjacent control runs. On each disposable process, only the background store thread moved from Linux nice 0 to nice 8 after startup. This experiment changes the replay environment and is separate from merge evidence.

The gate counts keys that arrived while a query was running.

| Replay | Keys | p95 | Max | Over 8 ms |
| --- | ---: | ---: | ---: | ---: |
| Weston, paced | 154 | 4.291 ms | 20.308 ms | 2 |
| Weston, burst | 265 | 5.043 ms | 11.337 ms | 1 |
| Sway, paced | 154 | 5.284 ms | 7.023 ms | 0 |

Weston still exceeded the 8 ms budget, so the worker-priority change needs stronger evidence before adopting it.
