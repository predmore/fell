//! The production store and conversation presentation, without a window.

use std::error::Error;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use fell::bench_read_conversation;
use fell_core::embed::{MINILM_L6_V2_INT8, OnnxEmbedder, default_models_dir, installed_model_dir};
use fell_core::test_support::mailbox::{self, LONG_MESSAGES, LONG_THREADS};
use fell_core::{FolderSpan, ListKey, SearchOptions, Store};
use serde::Deserialize;

const SAMPLES: usize = 31;
const PAGE_WARMUPS: usize = 10;
const PAGE_SAMPLES: usize = 101;
const THREAD_SAMPLES: usize = 5 * LONG_THREADS;
const WATCH_HEADROOM: f64 = 1.20;
const HYBRID_PRODUCT_TARGET_MS: f64 = 150.0;
const PAGE_SIZE: usize = 100;
const INBOX: &str = "INBOX";
const QUERIES: [(&str, &str, Option<&str>); 5] = [
    ("Single term", "invoice", None),
    ("Multiple terms", "boiler leak", None),
    ("Quoted phrase", "\"boarding pass\"", None),
    (
        "Text and operators",
        "warranty from:p10@bench.test in:inbox has:attachment after:2023-12-01",
        None,
    ),
    ("Misspelling", "invioce", Some("invoice")),
];
// (Search query, text passed to MiniLM after removing search operators.)
const HYBRID_QUERIES: [(&str, &str); 4] = [
    ("money owed next month", "money owed next month"),
    ("when does my flight leave", "when does my flight leave"),
    (
        "somewhere to stay for the trip",
        "somewhere to stay for the trip",
    ),
    ("in:Archive budget meeting", "budget meeting"),
];

/// Two thresholds per row. A watch value is a hosted-runner reference: a
/// row above it plus [`WATCH_HEADROOM`] is flagged but passes, because hosted
/// CPUs differ by more than that. A `*_max_*` value is a product target (or a
/// documented allowance above one) and fails the job.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Budgets {
    folder_page_p95_ms: f64,
    thread_open_p95_ms: f64,
    keyword_search_p95_ms: f64,
    semantic_residual_p95_ms: f64,
    semantic_graph_p95_ms: f64,
    rest_rss_mb: f64,
    folder_page_max_ms: f64,
    thread_open_max_ms: f64,
    keyword_search_max_ms: f64,
    hybrid_search_max_ms: f64,
    rss_max_mb: f64,
}

impl Budgets {
    fn read(path: &Path) -> Result<Self, Box<dyn Error>> {
        let budgets: Self = toml::from_str(&fs::read_to_string(path)?)?;
        // Compare each fail limit with the watch threshold its row applies.
        for (name, watch, max) in [
            (
                "folder_page",
                budgets.folder_page_p95_ms * WATCH_HEADROOM,
                budgets.folder_page_max_ms,
            ),
            (
                "thread_open",
                budgets.thread_open_p95_ms * WATCH_HEADROOM,
                budgets.thread_open_max_ms,
            ),
            (
                "keyword_search",
                budgets.keyword_search_p95_ms * WATCH_HEADROOM,
                budgets.keyword_search_max_ms,
            ),
            (
                "rss",
                budgets.rest_rss_mb * WATCH_HEADROOM,
                budgets.rss_max_mb,
            ),
            (
                "hybrid_search",
                HYBRID_PRODUCT_TARGET_MS,
                budgets.hybrid_search_max_ms,
            ),
        ] {
            if !(watch > 0.0 && max.is_finite() && watch <= max) {
                return Err(format!(
                    "{name}: watch values must be positive and below the fail limit in {}",
                    path.display()
                )
                .into());
            }
        }
        for (name, value) in [
            ("semantic_residual_p95_ms", budgets.semantic_residual_p95_ms),
            ("semantic_graph_p95_ms", budgets.semantic_graph_p95_ms),
        ] {
            if !(value > 0.0 && value.is_finite()) {
                return Err(
                    format!("{name} must be positive and finite in {}", path.display()).into(),
                );
            }
        }
        Ok(budgets)
    }
}

#[derive(Clone, Copy)]
struct Budget {
    /// Flag the row above this value; never fails.
    watch: Option<f64>,
    /// Fail the job above this value.
    max: Option<f64>,
}

impl Budget {
    fn new(reference: f64, max: f64) -> Self {
        Self {
            watch: Some(reference * WATCH_HEADROOM),
            max: Some(max),
        }
    }

    fn watch_only(reference: f64) -> Self {
        Self {
            watch: Some(reference * WATCH_HEADROOM),
            max: None,
        }
    }
}

fn percentile_ms(sorted: &[Duration], percentile: usize) -> f64 {
    sorted[(sorted.len() * percentile).div_ceil(100) - 1].as_secs_f64() * 1e3
}

fn cpuinfo_field<'a>(cpuinfo: &'a str, key: &str) -> Option<&'a str> {
    cpuinfo
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find_map(|(k, value)| (k.trim() == key).then_some(value.trim()))
}

fn cpu_report() -> String {
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    // x86 names the model; arm64 kernels usually report only implementer and part ids.
    let model = cpuinfo_field(&cpuinfo, "model name")
        .or_else(|| cpuinfo_field(&cpuinfo, "Model"))
        .or_else(|| cpuinfo_field(&cpuinfo, "Hardware"))
        .map(str::to_owned)
        .or_else(|| {
            let implementer = cpuinfo_field(&cpuinfo, "CPU implementer")?;
            let part = cpuinfo_field(&cpuinfo, "CPU part")?;
            Some(format!("implementer {implementer}, part {part}"))
        })
        .unwrap_or_else(|| "unavailable".into());
    let logical_cpus = std::thread::available_parallelism()
        .map_or_else(|_| "unavailable".into(), |count| count.to_string());
    let arch = std::env::consts::ARCH;
    let threads = OnnxEmbedder::testing_executor_threads();
    #[cfg_attr(not(target_arch = "x86_64"), allow(unused_mut))]
    let mut out = format!(
        "CPU model: {model} ({arch})\n\nLogical CPUs available: {logical_cpus}\n\nTract executor threads: {threads}\n\n"
    );
    #[cfg(target_arch = "x86_64")]
    {
        let avx = std::arch::is_x86_feature_detected!("avx");
        let avx2 = std::arch::is_x86_feature_detected!("avx2");
        let fma = std::arch::is_x86_feature_detected!("fma");
        let f16c = std::arch::is_x86_feature_detected!("f16c");
        let avx512f = std::arch::is_x86_feature_detected!("avx512f");
        writeln!(
            out,
            "x86 features: avx={avx} avx2={avx2} fma={fma} f16c={f16c} avx512f={avx512f}\n"
        )
        .unwrap();
        // MiniLM's integer matrix kernels need AVX2 for the fast tract path.
        // Every CPU keeps the same ceilings until a slow run shows the cause (#228).
        if !avx2 {
            writeln!(
                out,
                "AVX2 is unavailable, so tract uses its slower integer kernels.\n"
            )
            .unwrap();
        }
    }
    out
}

struct Report {
    out: String,
    failures: Vec<String>,
    watched: Vec<String>,
}

impl Report {
    fn new(cpu: &str) -> Self {
        Self {
            out: format!(
                "## 100k-message store budgets\n\n{cpu}\
                 | Operation | Samples | p50 (ms) | p95 (ms) or RSS (MB) | Watch | Fail above | Result |\n\
                 | --- | ---: | ---: | ---: | ---: | ---: | --- |\n",
            ),
            failures: Vec::new(),
            watched: Vec::new(),
        }
    }

    fn row(
        &mut self,
        label: &str,
        samples: usize,
        p50: Option<f64>,
        measured: f64,
        budget: Option<Budget>,
    ) {
        let format = |value: Option<f64>| value.map_or_else(|| "—".into(), |v| format!("{v:.3}"));
        let (watch, max, result) = match budget {
            Some(Budget { watch, max }) => {
                let result = if max.is_some_and(|max| measured > max) {
                    self.failures.push(label.into());
                    "**Fail**"
                } else if let Some(watch) = watch.filter(|watch| measured > *watch) {
                    // A GitHub Actions annotation; plain output elsewhere.
                    println!(
                        "::warning title=Store budget watch::{label}: {measured:.3} is above the {watch:.3} watch value"
                    );
                    self.watched.push(label.into());
                    "Watch"
                } else {
                    "Pass"
                };
                (format(watch), format(max), result)
            }
            None => ("—".into(), "—".into(), "Informational"),
        };
        let p50 = p50.map_or_else(|| "—".into(), |value| format!("{value:.3}"));
        writeln!(
            self.out,
            "| {label} | {samples} | {p50} | {measured:.3} | {watch} | {max} | {result} |"
        )
        .unwrap();
    }

    fn latency<T>(
        &mut self,
        label: &str,
        count: usize,
        budget: Option<Budget>,
        mut run: impl FnMut(usize) -> T,
    ) -> (f64, f64) {
        let mut samples: Vec<_> = (0..count)
            .map(|index| {
                let started = Instant::now();
                let result = black_box(run(index));
                let elapsed = started.elapsed();
                drop(result);
                elapsed
            })
            .collect();
        samples.sort_unstable();
        let p50 = percentile_ms(&samples, 50);
        let p95 = percentile_ms(&samples, 95);
        self.row(label, count, Some(p50), p95, budget);
        (p50, p95)
    }

    fn rss(&mut self, label: &str, budget: Budget) -> Result<(), Box<dyn Error>> {
        let stats = fs::read_to_string("/proc/self/smaps_rollup")?;
        let rss_kib: u64 = stats
            .lines()
            .find_map(|line| line.strip_prefix("Rss:"))
            .and_then(|rest| rest.split_whitespace().next())
            .ok_or("no Rss entry in /proc/self/smaps_rollup")?
            .parse()?;
        self.row(label, 1, None, rss_kib as f64 * 1024.0 / 1e6, Some(budget));
        Ok(())
    }

    fn finish(mut self) -> Result<(), Box<dyn Error>> {
        if !self.watched.is_empty() {
            writeln!(
                self.out,
                "\nAbove the hosted reference (watch only, not a failure): {}.",
                self.watched.join(", ")
            )?;
        }
        println!("{}", self.out);
        if let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?
                .write_all(self.out.as_bytes())?;
        }
        if self.failures.is_empty() {
            Ok(())
        } else {
            Err(format!("store budget exceeded: {}", self.failures.join(", ")).into())
        }
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1).filter(|arg| arg != "--bench");
    let result = match args.next() {
        None => run_suite(),
        Some(arg) if arg == "--install-model" => {
            let code = fell::embeddings_command(&["install".into(), "--yes".into()]);
            if code == 0 {
                Ok(())
            } else {
                Err(format!("pinned MiniLM installer exited with code {code}").into())
            }
        }
        Some(arg) if arg == "--seed" => {
            let path = PathBuf::from(args.next().expect("missing seed path"));
            mailbox::seed(&path).map_err(Into::into)
        }
        Some(arg) => Err(format!("unexpected argument {arg:?}").into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("fell-bench: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run_suite() -> Result<(), Box<dyn Error>> {
    let suite_started = Instant::now();
    let budget_path = std::env::var_os("FELL_BENCH_BUDGETS").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bench/budgets.toml"),
        PathBuf::from,
    );
    let budgets = Budgets::read(&budget_path)?;
    let executable = std::env::current_exe()?;
    if std::env::var_os("FELL_BENCH_INSTALL_MODEL").is_some_and(|value| value == "1") {
        let root = std::env::var_os("FELL_BENCH_MODEL_DIR")
            .ok_or("FELL_BENCH_MODEL_DIR is required when installing MiniLM")?;
        let status = Command::new(&executable)
            .arg("--install-model")
            .env("FELL_MODEL_DIR", root)
            .status()?;
        if !status.success() {
            return Err(format!("pinned MiniLM setup exited with {status}").into());
        }
    }
    let cpu = cpu_report();
    let mut report = Report::new(&cpu);
    let scratch = tempfile::Builder::new()
        .prefix("fell-store-budgets-")
        .tempdir()?;
    let path = scratch.path().join("mail.db");
    // The measurement process never allocates the writer's SQLite caches or
    // message batches. Closing the child also checkpoints its WAL.
    eprintln!("Generating the 100,000-message benchmark fixture");
    let seed_started = Instant::now();
    let status = Command::new(executable).arg("--seed").arg(&path).status()?;
    if !status.success() {
        return Err(format!("mailbox generation exited with {status}").into());
    }
    let seed_elapsed = seed_started.elapsed();
    let disk_mb = fs::metadata(&path)?.len() as f64 / 1e6;
    eprintln!("Seeded 100,000 messages in {seed_elapsed:.2?} ({disk_mb:.1} MB)");

    let index_lock = mailbox::hold_indexer(&path)?;
    let account = mailbox::account();
    let rss_budget = Budget::new(budgets.rest_rss_mb, budgets.rss_max_mb);
    let store = Store::open(&path)?;
    report.rss("RSS after store open", rss_budget)?;
    let top_span = FolderSpan::Top { rows: PAGE_SIZE };
    let top_page = store.folder_page(&account.id, INBOX, &top_span)?;
    assert_eq!(top_page.threads.len(), PAGE_SIZE);
    report.rss("RSS after first Inbox page", rss_budget)?;
    let roles = store.folder_roles(&account.id)?;
    let folders = store.folder_names(&account.id)?;
    let read = |store: &Store, index| {
        bench_read_conversation(
            store,
            &account.id,
            &mailbox::long_thread(index),
            (&account.email, &roles, &folders, INBOX),
        )
        .unwrap()
    };
    let mut last_view = None;
    for index in 0..LONG_THREADS {
        let view = read(&store, index);
        assert_eq!(view.len(), LONG_MESSAGES);
        last_view = Some(view);
    }
    report.rss("RSS after opening 20 distinct threads", rss_budget)?;
    black_box((top_page, last_view));

    // These timings open fresh connections over a recently seeded file. The
    // initial RSS sample above precedes repeated opens and list/search warmups.
    drop(store);
    report.latency("Open populated store", SAMPLES, None, |_| {
        Store::open(&path).unwrap()
    });
    let thread_budget = Budget::new(budgets.thread_open_p95_ms, budgets.thread_open_max_ms);
    let folder_budget = Budget::new(budgets.folder_page_p95_ms, budgets.folder_page_max_ms);
    let store = Store::open(&path)?;
    let mut measure_inbox_page = |label: &str, span: &FolderSpan| -> Result<(), Box<dyn Error>> {
        for _ in 0..PAGE_WARMUPS {
            let page = store.folder_page(&account.id, INBOX, span)?;
            assert_eq!(page.threads.len(), PAGE_SIZE);
            black_box(page);
        }
        report.latency(label, PAGE_SAMPLES, Some(folder_budget), |_| {
            store.folder_page(&account.id, INBOX, span).unwrap()
        });
        Ok(())
    };
    measure_inbox_page("Inbox first page (100 rows)", &top_span)?;
    let middle_span = {
        let full = store.folder_threads(&account.id, INBOX)?;
        let middle = full.len() / 2;
        FolderSpan::Window {
            first: Some(ListKey::of(&full[middle])),
            last: ListKey::of(&full[middle + PAGE_SIZE - 1]),
            before: 0,
            after: 0,
        }
    };
    measure_inbox_page("Inbox page near middle cursor (100 rows)", &middle_span)?;
    let (label_page, _) = store.label_page(&account.id, "tag1", &top_span, None)?;
    assert_eq!(label_page.threads.len(), PAGE_SIZE);
    assert!(label_page.more_after);
    report.latency(
        "Label page (100 rows)",
        SAMPLES,
        Some(folder_budget),
        |_| {
            (
                store
                    .label_page(&account.id, "tag1", &top_span, None)
                    .unwrap(),
                store.list_labels(&account.id).unwrap(),
            )
        },
    );
    for index in 0..LONG_THREADS {
        black_box(read(&store, index));
    }
    report.latency(
        "Read and present a 50-message thread",
        THREAD_SAMPLES,
        Some(thread_budget),
        |index| read(&store, index % LONG_THREADS),
    );

    for (shape, query, correction) in QUERIES {
        let options = SearchOptions { semantic: false };
        let warm = store.search_with(&account.id, query, options)?;
        assert!(
            !warm.hits.is_empty(),
            "benchmark query returned no hits: {query}"
        );
        assert_eq!(warm.did_you_mean.as_deref(), correction, "{query}");
        let label = format!("Keyword · {shape} (`{query}`)");
        report.latency(
            &label,
            SAMPLES,
            Some(Budget::new(
                budgets.keyword_search_p95_ms,
                budgets.keyword_search_max_ms,
            )),
            |_| store.search_with(&account.id, query, options).unwrap(),
        );
    }
    let semantic = measure_semantic(&store, &mut report, &budgets);
    let semantic_note = match &semantic {
        Ok(note) => note.clone(),
        Err(err) => format!("Hybrid measurement failed: {err}"),
    };
    // Include cleanup in the reported suite time.
    drop(store);
    drop(index_lock);
    scratch.close()?;
    writeln!(
        report.out,
        "\n{semantic_note}\n\nFixture generation took {seed_elapsed:.2?} ({disk_mb:.1} MB on disk). Full suite took {:.2?}, including setup and cleanup. Latency rows use nearest-rank percentiles. RSS values are single process snapshots in decimal MB.",
        suite_started.elapsed()
    )?;
    report.finish()?;
    semantic.map(|_| ())
}

fn measure_semantic(
    store: &Store,
    report: &mut Report,
    budgets: &Budgets,
) -> Result<String, Box<dyn Error>> {
    let explicit = std::env::var_os("FELL_BENCH_MODEL_DIR").map(PathBuf::from);
    let root = explicit.clone().or_else(default_models_dir);
    let Some(dir) = root
        .as_deref()
        .and_then(|root| installed_model_dir(&MINILM_L6_V2_INT8, root))
    else {
        if let Some(root) = explicit {
            return Err(format!("pinned MiniLM files missing under {}", root.display()).into());
        }
        return Ok("Hybrid search skipped because the pinned local MiniLM model is absent.".into());
    };
    // The watch value is the product target; the fail limit sits above it
    // because some CPUs take a much slower MiniLM path (#228, #237).
    let hybrid_budget = Budget {
        watch: Some(HYBRID_PRODUCT_TARGET_MS),
        max: Some(budgets.hybrid_search_max_ms),
    };
    let graph_budget = Budget::watch_only(budgets.semantic_graph_p95_ms);
    let started = Instant::now();
    let model = OnnxEmbedder::load(&MINILM_L6_V2_INT8, &dir)?;
    mailbox::seed_vectors(store, &model)?;
    let mut setup = started.elapsed();
    let kernels = model.testing_matmul_kernels().join(", ");
    let kernels = if kernels.is_empty() {
        "(none)".to_owned()
    } else {
        kernels
    };
    let mut graph_p95s = Vec::with_capacity(HYBRID_QUERIES.len());
    for (query, text) in HYBRID_QUERIES {
        // Warm this input shape before timing the graph.
        model.testing_graph_latency(text)?;
        let mut samples = (0..SAMPLES)
            .map(|_| model.testing_graph_latency(text))
            .collect::<Result<Vec<_>, _>>()?;
        samples.sort_unstable();
        let p95 = percentile_ms(&samples, 95);
        report.row(
            &format!("MiniLM graph · `{query}`"),
            samples.len(),
            Some(percentile_ms(&samples, 50)),
            p95,
            Some(graph_budget),
        );
        graph_p95s.push(p95);
    }
    let started = Instant::now();
    store.testing_search_embedder(Box::new(model))?;
    let account = mailbox::account();
    assert!(store.semantic_search_active());
    for (query, _) in HYBRID_QUERIES {
        let warm = store.search(&account.id, query)?;
        assert!(warm.hits.iter().any(|hit| hit.semantic_only), "{query}");
    }
    setup += started.elapsed();
    let mut above_product_target = 0;
    for ((query, _), graph_p95) in HYBRID_QUERIES.into_iter().zip(graph_p95s) {
        let (_, hybrid_p95) = report.latency(
            &format!("Hybrid · `{query}`"),
            SAMPLES,
            Some(hybrid_budget),
            |_| store.search(&account.id, query).unwrap(),
        );
        above_product_target += usize::from(hybrid_p95 > HYBRID_PRODUCT_TARGET_MS);
        report.row(
            &format!("Hybrid residual · `{query}`"),
            SAMPLES,
            None,
            (hybrid_p95 - graph_p95).max(0.0),
            Some(Budget::watch_only(budgets.semantic_residual_p95_ms)),
        );
    }
    Ok(format!(
        "Hybrid setup took {setup:.2?}, including model load, 100k-vector seeding and search warmup.\n\n\
         Tract matrix kernels in the optimized graph: {kernels}\n\n\
         Hybrid p95 exceeded the {HYBRID_PRODUCT_TARGET_MS:.0} ms product target for {above_product_target} of {} queries. Each residual is the difference between the query's end-to-end p95 and graph p95, floored at zero.",
        HYBRID_QUERIES.len()
    ))
}
