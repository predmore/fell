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
use rusqlite::Connection;
use serde::Deserialize;

const SAMPLES: usize = 31;
const PAGE_WARMUPS: usize = 10;
const PAGE_SAMPLES: usize = 101;
const THREAD_SAMPLES: usize = 5 * LONG_THREADS;
const SQLITE_CONTROL_ITERATIONS: usize = 2_048;
const HEADROOM: f64 = 1.20;
const MAX_THREAD_SCALE: f64 = 1.25;
const HYBRID_PRODUCT_MAX_MS: f64 = 150.0;
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Budgets {
    folder_page_p95_ms: f64,
    thread_open_p95_ms: f64,
    thread_reference_sqlite_p50_ms: f64,
    keyword_search_p95_ms: f64,
    semantic_residual_p95_ms: f64,
    semantic_graph_p95_ms: f64,
    rest_rss_mb: f64,
}

impl Budgets {
    fn read(path: &Path) -> Result<Self, Box<dyn Error>> {
        let budgets: Self = toml::from_str(&fs::read_to_string(path)?)?;
        for (name, value) in [
            ("folder_page_p95_ms", budgets.folder_page_p95_ms),
            ("thread_open_p95_ms", budgets.thread_open_p95_ms),
            (
                "thread_reference_sqlite_p50_ms",
                budgets.thread_reference_sqlite_p50_ms,
            ),
            ("keyword_search_p95_ms", budgets.keyword_search_p95_ms),
            ("semantic_residual_p95_ms", budgets.semantic_residual_p95_ms),
            ("semantic_graph_p95_ms", budgets.semantic_graph_p95_ms),
            ("rest_rss_mb", budgets.rest_rss_mb),
        ] {
            let max_limit = value * HEADROOM * MAX_THREAD_SCALE;
            if !(value > 0.0 && max_limit.is_finite()) {
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
    baseline: f64,
    limit: f64,
}

impl Budget {
    fn fixed(baseline: f64) -> Self {
        Self {
            baseline,
            limit: baseline * HEADROOM,
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
}

impl Report {
    fn new(cpu: &str) -> Self {
        Self {
            out: format!(
                "## 100k-message store budgets\n\n{cpu}\
                 | Operation | Samples | p50 (ms) | p95 (ms) or RSS (MB) | Baseline | Limit | Result |\n\
                 | --- | ---: | ---: | ---: | ---: | ---: | --- |\n",
            ),
            failures: Vec::new(),
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
        let (base, limit, result) = match budget {
            Some(Budget { baseline, limit }) => {
                let result = if measured <= limit {
                    "Pass"
                } else {
                    self.failures.push(label.into());
                    "**Fail**"
                };
                (format!("{baseline:.3}"), format!("{limit:.3}"), result)
            }
            None => ("—".into(), "—".into(), "Informational"),
        };
        let p50 = p50.map_or_else(|| "—".into(), |value| format!("{value:.3}"));
        writeln!(
            self.out,
            "| {label} | {samples} | {p50} | {measured:.3} | {base} | {limit} | {result} |"
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

    fn rss(&mut self, label: &str, budget: f64) -> Result<(), Box<dyn Error>> {
        let stats = fs::read_to_string("/proc/self/smaps_rollup")?;
        let rss_kib: u64 = stats
            .lines()
            .find_map(|line| line.strip_prefix("Rss:"))
            .and_then(|rest| rest.split_whitespace().next())
            .ok_or("no Rss entry in /proc/self/smaps_rollup")?
            .parse()?;
        self.row(
            label,
            1,
            None,
            rss_kib as f64 * 1024.0 / 1e6,
            Some(Budget::fixed(budget)),
        );
        Ok(())
    }

    fn finish(self) -> Result<(), Box<dyn Error>> {
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
    let store = Store::open(&path)?;
    report.rss("RSS after store open", budgets.rest_rss_mb)?;
    let top_span = FolderSpan::Top { rows: PAGE_SIZE };
    let top_page = store.folder_page(&account.id, INBOX, &top_span)?;
    assert_eq!(top_page.threads.len(), PAGE_SIZE);
    report.rss("RSS after first Inbox page", budgets.rest_rss_mb)?;
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
    report.rss("RSS after opening 20 distinct threads", budgets.rest_rss_mb)?;
    black_box((top_page, last_view));

    // These timings open fresh connections over a recently seeded file. The
    // initial RSS sample above precedes repeated opens and list/search warmups.
    drop(store);
    let control_p50 = measure_sqlite_control(&mut report)?;
    report.latency("Open populated store", SAMPLES, None, |_| {
        Store::open(&path).unwrap()
    });
    let thread_scale =
        (control_p50 / budgets.thread_reference_sqlite_p50_ms).clamp(1.0, MAX_THREAD_SCALE);
    let mut thread_budget = Budget::fixed(budgets.thread_open_p95_ms);
    thread_budget.limit *= thread_scale;
    let folder_budget = Budget::fixed(budgets.folder_page_p95_ms);
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
            Some(Budget::fixed(budgets.keyword_search_p95_ms)),
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
        "\nThread limit scaled by {thread_scale:.3} using the in-memory SQLite control p50.\n\n{semantic_note}\n\nFixture generation took {seed_elapsed:.2?} ({disk_mb:.1} MB on disk). Full suite took {:.2?}, including setup and cleanup. Latency rows use nearest-rank percentiles. RSS values are single process snapshots in decimal MB.",
        suite_started.elapsed()
    )?;
    report.finish()?;
    semantic.map(|_| ())
}

fn measure_sqlite_control(report: &mut Report) -> rusqlite::Result<f64> {
    let conn = Connection::open_in_memory()?;
    let mut stmt = conn.prepare("SELECT 1")?;
    let label = format!("SQLite control ({SQLITE_CONTROL_ITERATIONS} × `SELECT 1`)");
    let (p50, _) = report.latency(&label, SAMPLES, None, |_| {
        for _ in 0..SQLITE_CONTROL_ITERATIONS {
            black_box(stmt.query_row([], |row| row.get::<_, i64>(0)).unwrap());
        }
    });
    Ok(p50)
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
    let hybrid_budget = Budget {
        baseline: HYBRID_PRODUCT_MAX_MS,
        limit: HYBRID_PRODUCT_MAX_MS,
    };
    let graph_budget = Budget::fixed(budgets.semantic_graph_p95_ms);
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
        above_product_target += usize::from(hybrid_p95 > HYBRID_PRODUCT_MAX_MS);
        report.row(
            &format!("Hybrid residual · `{query}`"),
            SAMPLES,
            None,
            (hybrid_p95 - graph_p95).max(0.0),
            Some(Budget::fixed(budgets.semantic_residual_p95_ms)),
        );
    }
    Ok(format!(
        "Hybrid setup took {setup:.2?}, including model load, 100k-vector seeding and search warmup.\n\n\
         Tract matrix kernels in the optimized graph: {kernels}\n\n\
         Hybrid p95 exceeded the {HYBRID_PRODUCT_MAX_MS:.0} ms product target for {above_product_target} of {} queries. Each residual is the difference between the query's end-to-end p95 and graph p95, floored at zero.",
        HYBRID_QUERIES.len()
    ))
}
