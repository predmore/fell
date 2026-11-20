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

The initial entries use the published targets. Compare several `ubuntu-latest` job summaries to those targets before adjusting the entries. Record the runner, representative p95s and RSS, and the job link when calibrating. Check the full job duration, including dependency setup and compilation, against ten minutes. [Issue #125](https://github.com/predmore/fell/issues/125) · [Product scope](../docs/scope.md)

To prove the failure path with a stricter threshold, run the same suite with a temporary budget file.

```sh
strict_budgets=$(mktemp)
sed 's/^keyword_search_p95_ms = .*/keyword_search_p95_ms = 0.001/' \
    bench/budgets.toml > "$strict_budgets"
FELL_BENCH_BUDGETS="$strict_budgets" \
    cargo bench --locked -p fell --features bench-store --bench fell-bench
rm "$strict_budgets"
```

The benchmark should exit with an error and show failing keyword rows in the table. On GitHub Actions the table also appears under the step summary.
