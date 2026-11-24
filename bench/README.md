# Store performance budgets

Run the release-mode gate on Linux.

```sh
cargo bench --locked -p fell --features bench-store --bench fell-bench
```

The benchmark builds a fresh SQLite mailbox of 100,000 distinct messages through `Store::apply_delta`. It uses synthetic threads of 1–80 messages, Sent/list copies, labels and text bodies of 0.5–20 KB. The fixture writer defers FTS updates until all copies are stored, then uses the store's normal rebuild to create the index and restore its triggers. Leave room for the database and its WAL under `$TMPDIR`. Measurements open fresh connections, and the writer runs in a child process so its allocations are outside the RSS measurement. An exclusive embedding-worker lock keeps the fixture idle during measurement.

The suite reports wall-clock p50 and p95 using nearest rank, with 31 samples per latency row by default. The first page and mid-folder cursor window each fetch 100 Inbox rows. Each window gets 10 untimed reads followed by 101 timed reads on the measurement connection. Its p95 is the sixth-slowest sample, so up to five slow outliers can exceed the limit without failing the row. The conversation benchmark warms 20 distinct 50-message threads on the measurement connection, then times five full cycles (100 samples). It calls the same read, body-state lookup and initial presentation code as the app. Timing stops before GPUI layout and paint or asynchronous HTML preparation. Store open creates a new connection over the seeded file with the OS page cache intact. RSS is sampled after the first open, after the first page, and after sequentially opening 20 threads while retaining the current conversation and the first page.

Keyword search runs all five query shapes with hybrid ranking disabled. If pinned MiniLM files are installed in Fell's model directory or in `$FELL_BENCH_MODEL_DIR`, the suite also measures hybrid ranking with real local query inference and 100,000 seeded vectors. It embeds the fixture's topic prefixes and adds deterministic jitter to their vectors to keep vector setup bounded. This models query cost and vector cardinality. The suite reads model files from local disk and reports a skip when the default installation is absent. CI sets `FELL_BENCH_INSTALL_MODEL=1` and passes an explicit model root. The benchmark calls Fell's installer in a child process before seeding, and a model setup or verification failure fails the job. To exercise the same hybrid benchmark locally, use `fell embeddings install` and set `FELL_BENCH_MODEL_DIR` to the model root reported by Fell.

## How the gate decides

Each row has two thresholds in [`budgets.toml`](budgets.toml).

- **Fail above.** The product target from [docs/scope.md](../docs/scope.md) and the table below. A row above it fails the job. Hosted runners measure 2.5× or more below every fail limit, so a failure means Fell got slower, not that the runner did.
- **Watch.** A hosted-runner reference value plus 20%. A row above it is marked **Watch**, gets a GitHub warning annotation, and is listed under the table, but the job passes. Hosted runner CPUs differ by more than 20% (an AMD EPYC 7763 runner measured the Inbox middle page at 0.93 ms where Intel Xeon 8573C runners measure 0.62–0.66 ms), so a tight reference can only be a hint. Look at Watch rows when a PR touches the store, search, or presentation code.

The gate used to fail at the reference values. It flaked on runner hardware again and again (#220, #228, #247, and #278), so only product targets fail now. Store open is informational because the 300 ms product budget also covers app startup and rendering. The job publishes the table before returning a failure.

Reference measurements come from the original `ubuntu-latest` (Ubuntu 26.04 x64) calibration at commit `422d92a`, and hybrid measurements from the optimized hosted run linked below. [Calibration job](https://github.com/predmore/fell/actions/runs/44645540660/job/135987038248)

| Metric | Hosted Ubuntu p95 or RSS | Watch (reference + 20%) | Fail above |
| --- | ---: | ---: | ---: |
| Folder page p95 | 0.568 ms | 0.90 ms | 5 ms |
| 50-message thread p95 | 2.404 ms | 3.60 ms | 16 ms |
| Keyword search p95 | 19.285 ms | 30.00 ms | 50 ms |
| RSS | 13.365 MB | 21.60 MB | 250 MB |
| Hybrid search p95 per query | 14.862–16.633 ms | 150 ms | 300 ms |
| MiniLM graph p95 per query | 4.098–6.441 ms | 72 ms | — |
| Hybrid residual p95 difference per query | 9.474–11.250 ms | 90 ms | — |

- **Hybrid search.** The product target is 150 ms, which is the Watch value. The fail limit is twice that because some hosted CPUs take a MiniLM path about 30× slower than others and measured 172–179 ms end to end with no change to Fell ([Issue #228](https://github.com/predmore/fell/issues/228), [Issue #237](https://github.com/predmore/fell/issues/237)). The graph and residual rows only watch. Warm every query and its graph input, then collect 31 end-to-end and 31 standalone MiniLM samples per query. Hybrid setup time covers model load, vector seeding and search warmup. The report counts queries whose end-to-end p95 exceeds the 150 ms product target. [Optimized hosted job](https://github.com/predmore/fell/actions/runs/44856382153/job/136701473396)
- **CPU diagnostics.** Every report starts with the CPU model, logical CPUs, runtime x86 feature flags (avx, avx2, fma, f16c, avx512f), tract's thread count and the matrix kernels in the optimized MiniLM graph. Check them first when a row is marked Watch.

CI builds the benchmark executable in a prerequisite job and passes it to the budget job from the same run. The suite itself takes under ten minutes. Both jobs have extra time and apt retries because a slow Ubuntu mirror once took 20 minutes to install build dependencies.

## Verify the regression gate

In a detached local worktree, run the command above with the checked-in budgets. To check paging, temporarily add `std::thread::sleep(std::time::Duration::from_millis(6));` at the entry to `Store::folder_page` in `crates/fell-core/src/store.rs` and rerun. Both Inbox latency rows should fail. A 0.5 ms sleep should mark them Watch and still pass. Restore paging before running the search checks below.

Local aarch64 runs can exceed the 21.60 MB RSS watch value. That marks the RSS rows Watch and does not fail.

To check search, temporarily add these delays at the entry to `Store::search_with` in `crates/fell-core/src/store.rs` and rerun.

```rust
if !options.semantic
    && query == "warranty from:p10@bench.test in:inbox has:attachment after:2023-12-01"
{
    std::thread::sleep(std::time::Duration::from_millis(60));
}
if options.semantic && query == "in:Archive budget meeting" {
    std::thread::sleep(std::time::Duration::from_millis(300));
}
```

The keyword text-and-operators row and the filtered hybrid row should fail. A thread-read delay over 16 ms should fail that row. Remove the temporary worktree after the checks.

## Replay the inbox

Build the release app and seed the same synthetic mailbox used by the store benchmark. The database path for `--seed` must be new.

```sh
cargo build --locked --release -p fell
cargo bench --locked -p fell --features bench-store --bench fell-bench -- --seed /tmp/fell-100k.db
python3 bench/inbox-replay.py target/release/fell /tmp/fell-100k.db /tmp/fell-ui-run
```

The replay needs Python 3, `xvfb-run`, Xvfb, xauth, xdotool, and Weston with its X11 backend and kiosk shell. Fell also needs a working Vulkan driver. The script starts a fresh X server and nested Wayland compositor, backs up the fixture into a disposable store, and uses separate XDG directories. Once Fell confirms it opened the store, the script sends 200 `j` keys, opens the conversation, scrolls with `Ctrl-d`, returns to the list, archives, and undoes. Add `--search` on a build with live search to send `/` and five query characters. The new output directory keeps the trace and both process logs. The fixture must be quiescent: no process may have it open, and it must be fully checkpointed, as `--seed` leaves it. The script reads it with SQLite's immutable mode, which takes no lock and ignores frames still in the WAL, so it refuses a fixture whose `-wal` file holds frames.

Startup is measured from entry to Fell's `main`. The trace reports window creation, first CPU paint, the first painted folder page, and the first confirmed Wayland presentation of each relevant stage. Presentation can be unavailable when the compositor lacks `wp_presentation`, uses an unsupported clock, or discards a commit. The replay prints sample coverage and nearest-rank p95 for both CPU paint and compositor presentation. Keyboard timing begins at the inbox handler. The synthetic store backup warms the OS page cache, and the nested compositor measures its own output.

To check a runner against a calibrated tolerance, pass `--ci-factor FACTOR`. The command requires complete presentation feedback and fails when usable inbox presentation exceeds `300 ms × FACTOR` or key-to-presentation p95 exceeds `8 ms × FACTOR`. For a regression check, temporarily sleep for 30 ms on each `j` at the start of `Inbox::on_key_down` after starting its trace timer, rebuild, and run both builds against the same fixture and factor. Use a native Wayland session with `fell --trace` for supported-hardware measurements.
