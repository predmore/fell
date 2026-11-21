# Store performance budgets

Run the release-mode gate on Linux.

```sh
cargo bench --locked -p fell --features bench-store --bench fell-bench
```

The benchmark builds a fresh SQLite mailbox of 100,000 distinct messages through `Store::apply_delta`. It uses synthetic threads of 1–80 messages, Sent/list copies, labels and text bodies of 0.5–20 KB. The fixture writer defers FTS updates until all copies are stored, then uses the store's normal rebuild to create the index and restore its triggers. Leave room for the database and its WAL under `$TMPDIR`. Measurements open fresh connections, and the writer runs in a child process so its allocations are outside the RSS measurement. An exclusive embedding-worker lock keeps the fixture idle during measurement.

The suite reports wall-clock p50 and p95 using nearest rank, with 31 samples per latency row by default. The first page and mid-folder cursor window each fetch 100 Inbox rows. The conversation benchmark warms 20 distinct 50-message threads on the measurement connection, then times five full cycles (100 samples). It calls the same read, body-state lookup and initial presentation code as the app. Timing stops before GPUI layout and paint or asynchronous HTML preparation. Store open creates a new connection over the seeded file with the OS page cache intact. RSS is sampled after the first open, after the first page, and after sequentially opening 20 threads while retaining the current conversation and the first page.

Keyword search runs all five query shapes with hybrid ranking disabled. If pinned MiniLM files are installed in Fell's model directory or in `$FELL_BENCH_MODEL_DIR`, the suite also measures hybrid ranking with real local query inference and 100,000 seeded vectors. It embeds the fixture's topic prefixes and adds deterministic jitter to their vectors to keep vector setup bounded. This models query cost and vector cardinality. The suite reads model files from local disk and reports a skip when the default installation is absent. CI sets `FELL_BENCH_INSTALL_MODEL=1` and passes an explicit model root. The benchmark calls Fell's installer in a child process before seeding, and a model setup or verification failure fails the job. To exercise the same hybrid benchmark locally, use `fell embeddings install` and set `FELL_BENCH_MODEL_DIR` to the model root reported by Fell.

## Calibrate the gate

The gate compares measured p95 latency or RSS in decimal MB to 120% of its baseline plus any adjustment below. The report shows measured times and effective limits. Store open is informational because the 300 ms product budget also covers app startup and rendering. The job publishes the table before returning a failure when a budget is exceeded. [`budgets.toml`](budgets.toml)

The original calibration uses the slowest measured p95 or largest RSS snapshot for each row on `ubuntu-latest` (Ubuntu 26.04 x64), commit `422d92a`, and the first model-backed hosted run. Product targets are measured on mid-range laptop hardware. [Calibration job](https://github.com/predmore/fell/actions/runs/44645540660/job/135987038248) · [Model-backed job](https://github.com/predmore/fell/actions/runs/44654557591/job/136017472591) · [Product scope](../docs/scope.md)

| Metric | Original Ubuntu p95 or RSS | Configured baseline | CI limit before adjustment | Product target |
| --- | ---: | ---: | ---: | ---: |
| Folder page p95 | 0.568 ms | 0.75 ms | 0.90 ms | 5 ms |
| 50-message thread p95 | 2.404 ms | 3.00 ms | 3.60 ms | 16 ms |
| Keyword search p95 | 19.285 ms | 25.00 ms | 30.00 ms | 50 ms |
| RSS | 13.365 MB | 18.00 MB | 21.60 MB | 250 MB |
| Hybrid search p95 per query | 106.841 ms (pooled) | 75 ms | 90 ms | 150 ms |

- **Thread read.** Multiply the 3.6 ms limit by the ratio of this run's store-open p50 to 1.85 ms, with the multiplier bounded from 1.0 to 1.25. The two hosted runs that missed the thread budget also took 2.26–2.32 ms at p50 to open the same store. The adjusted thread limit can reach 4.5 ms. [Issue #220](https://github.com/predmore/fell/issues/220)
- **Hybrid search.** Warm every query and check their p95s individually. For each query, measure 11 standalone runs of the pinned MiniLM graph at the same token length. Add the graph p50 above 20 ms to that query's 90 ms limit, with at most 200 ms extra. This allows for the observed spread in MiniLM throughput while preserving the full wall-clock measurement, including inference, SQLite, and vector ranking. Because the graph is shared with production, a slowdown inside that graph affects the allowance up to its cap. The table shows the graph and end-to-end timings so both remain visible. The 90 ms starting limit reflects the removal of the common-filter mailbox scan. [Issue #220](https://github.com/predmore/fell/issues/220)

The calibration job took 9m54s, including 62s to save an 848 MB cache, 1m24s to build, and 279s to seed the mailbox. A later cold release build exceeded the ten-minute job limit before producing an executable. CI now builds that executable in a prerequisite job and passes it to the budget job from the same run. The ten-minute budget job covers runtime setup, model verification, fixture generation and measurements. The full CI path includes the prerequisite build. [Cold build](https://github.com/predmore/fell/actions/runs/44661013350/job/136038979928)

## Verify the regression gate

In a detached local worktree, run the command above with the checked-in budgets. Then temporarily add this delay at the entry to `Store::search_with` in `crates/fell-core/src/store.rs` and run the same command again.

```rust
if !options.semantic
    && query == "warranty from:p10@bench.test in:inbox has:attachment after:2023-12-01"
{
    std::thread::sleep(std::time::Duration::from_millis(100));
}
if options.semantic && query == "in:Archive budget meeting" {
    std::thread::sleep(std::time::Duration::from_millis(100));
}
```

The keyword text-and-operators row and the filtered hybrid row should fail their budgets. The hybrid allowance is based on a standalone graph run, which bypasses this added delay. Remove the temporary worktree after the check.
