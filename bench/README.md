# Store performance budgets

Run the same release-mode gate as CI on Linux.

```sh
cargo bench --locked -p fell --features bench-store --bench fell-bench
```

The benchmark builds a fresh SQLite mailbox of 100,000 distinct messages through `Store::apply_delta`. It uses synthetic threads of 1–80 messages, Sent/list copies, labels and text bodies of 0.5–20 KB. Leave room for the database and its WAL under `$TMPDIR`. Fixture generation runs in a child process so its allocations are outside the RSS measurement. An exclusive embedding-worker lock keeps the fixture idle during measurement.

The suite reports p50 and p95 over 31 samples using nearest rank. The first page and mid-folder cursor window each fetch 100 Inbox rows. The conversation benchmark cycles through 20 distinct 50-message threads and calls the same read, body-state lookup and initial presentation code as the app. Timing stops before GPUI layout and paint or asynchronous HTML preparation. Store open creates a new connection over the seeded file with the OS page cache intact. RSS is sampled after the first open, after the first page, and after sequentially opening 20 threads while retaining the current conversation and the first page.

Keyword search runs all five query shapes with hybrid ranking disabled. If pinned MiniLM files are installed in Fell's model directory or in `$FELL_BENCH_MODEL_DIR`, the suite also measures hybrid ranking with real local query inference and 100,000 seeded vectors. It embeds the fixture's topic prefixes and adds deterministic jitter to their vectors to keep vector setup bounded. This models query cost and vector cardinality. The suite reads model files from local disk and reports a skip when the default installation is absent.

## Calibrate the gate

Use p95 for each latency budget and decimal MB for RSS. The gate checks each timed shape or checkpoint against 120% of its baseline. Store open is informational because the 300 ms product budget also covers app startup and rendering. The job publishes the table before returning a failure when a budget is exceeded. [`budgets.toml`](budgets.toml)

The calibration uses the slowest measured p95 or largest RSS snapshot for each row on `ubuntu-latest` (Ubuntu 26.04 x64), commit `422d92a`. The configured baselines give the measured values roughly 25–35% room for runner variance, before the gate adds its 20% tolerance. The published product maxima still apply. Hybrid keeps its original 150 ms baseline until a comparable runner measures it with the pinned MiniLM model. [Calibration job](https://github.com/predmore/fell/actions/runs/44645540660/job/135987038248) · [Issue #125](https://github.com/predmore/fell/issues/125) · [Product scope](../docs/scope.md)

| Metric | Measured Ubuntu maximum | Configured baseline | CI limit (+20%) | Product maximum |
| --- | ---: | ---: | ---: | ---: |
| Folder page p95 | 0.568 ms | 0.75 ms | 0.90 ms | 5 ms |
| 50-message thread p95 | 2.404 ms | 3.00 ms | 3.60 ms | 16 ms |
| Keyword search p95 | 19.285 ms | 25.00 ms | 30.00 ms | 50 ms |
| RSS | 13.365 MB | 18.00 MB | 21.60 MB | 250 MB |
| Hybrid search p95 | Skipped | 150 ms | 180 ms | 150 ms |

The calibration job took 9m54s, including 62s to save an 848 MB cache, 1m24s to build, and 279s to seed the mailbox. The budget job now restores the release cache from the Linux artifacts workflow on `main` and installs the required build packages without their recommendations. Skipping the cache save alone would have left about 68s under the timeout in that run. Recheck the hosted duration and the baselines across subsequent runs.

## Verify the regression gate

In a detached local worktree, run the command above with the checked-in budgets. Then temporarily add this delay at the entry to `Store::search_with` in `crates/fell-core/src/store.rs` and run the same command again.

```rust
if !options.semantic
    && query == "warranty from:p10@bench.test in:inbox has:attachment after:2023-12-01"
{
    std::thread::sleep(std::time::Duration::from_millis(100));
}
```

The text-and-operators row should exceed the 30 ms CI limit and the process should exit with a failure. Remove the temporary worktree after the check.
