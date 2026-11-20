# Recovery after search results change

Before captures use release `7dbd7df48d7385d56cce7effd3034a331b732d1a`. After captures use release `7261b6b7a6134088949b5522ccd07a0beeb17a71`. The replays used 1200 × 780 Tokyo Night windows, private Weston sessions, and disposable synthetic mailboxes.

## Unread conversation during a star write

The replay set a 700 ms read dwell. After locating the unread dispatch, a separate connection held a SQLite write transaction for 1.8 seconds. The reader opened the dispatch and starred it while the transaction was held. The dwell expired before the write and search refresh could complete.

| Release | Original unread / starred | Stored unread / starred after refresh |
| --- | --- | --- |
| `7dbd7df` | 1 / 0 | 1 / 1 |
| `7261b6b` | 1 / 0 | 0 / 1 |

[Before](dwell-before.png) · [After](dwell-after.png)

## A hit disappears before the reader opens it

The replay loaded two results for `has:attachment`, then removed Tickets for Friday from the fixture through another connection. Selecting that result on the old build left two rows and an indefinite Updating preview cue. The new build dropped the vanished hit and recovered to the other result.

[Before](vanished-before.png) · [After](vanished-after.png)

## Frame timing

The same hardware-accelerated Sway replay as the previous release used the Apple M2 and 100,000-message fixture. All 154 keystrokes overlapping query work stayed within 8 ms from handler entry to CPU paint. The p95 was 2.549 ms and the maximum was 4.085 ms. One key outside the query intervals took 9.277 ms. The replay did not measure display presentation.

[Raw frame samples](frames.jsonl) · [Timing summary](frame-stats.txt)
