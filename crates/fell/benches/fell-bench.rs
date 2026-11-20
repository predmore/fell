//! The production store and conversation presentation, without a window.

use std::error::Error;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use fell::bench_read_conversation;
use fell_core::embed::{MINILM_L6_V2_INT8, OnnxEmbedder, default_models_dir, installed_model_dir};
use fell_core::test_support::mailbox::{self, LONG_MESSAGES, LONG_THREADS};
use fell_core::{FolderSpan, ListKey, SearchOptions, Store};
use serde::Deserialize;

const SAMPLES: usize = 31;
const HEADROOM: f64 = 1.20;
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
const HYBRID_QUERIES: [&str; 4] = [
    "money owed next month",
    "when does my flight leave",
    "somewhere to stay for the trip",
    "in:Archive budget meeting",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Budgets {
    folder_page_p95_ms: f64,
    thread_open_p95_ms: f64,
    keyword_search_p95_ms: f64,
    semantic_search_p95_ms: f64,
    rest_rss_mb: f64,
}

impl Budgets {
    fn read(path: &Path) -> Result<Self, Box<dyn Error>> {
        let budgets: Self = toml::from_str(&fs::read_to_string(path)?)?;
        for (name, value) in [
            ("folder_page_p95_ms", budgets.folder_page_p95_ms),
            ("thread_open_p95_ms", budgets.thread_open_p95_ms),
            ("keyword_search_p95_ms", budgets.keyword_search_p95_ms),
            ("semantic_search_p95_ms", budgets.semantic_search_p95_ms),
            ("rest_rss_mb", budgets.rest_rss_mb),
        ] {
            if !(value.is_finite() && value > 0.0 && (value * HEADROOM).is_finite()) {
                return Err(
                    format!("{name} must be positive and finite in {}", path.display()).into(),
                );
            }
        }
        Ok(budgets)
    }
}

struct Report {
    out: String,
    failures: Vec<String>,
}

impl Report {
    fn new() -> Self {
        Self {
            out: String::from(
                "## 100k-message store budgets\n\n\
                 | Operation | Samples | p50 (ms) | p95 (ms) or RSS (MB) | Baseline | Limit (+20%) | Result |\n\
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
        budget: Option<f64>,
    ) {
        let (base, limit, result) = match budget {
            Some(baseline) => {
                let limit = baseline * HEADROOM;
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

    fn latency<T>(&mut self, label: &str, budget: Option<f64>, mut run: impl FnMut(usize) -> T) {
        let mut samples: Vec<_> = (0..SAMPLES)
            .map(|index| {
                let started = Instant::now();
                let result = black_box(run(index));
                let elapsed = started.elapsed();
                drop(result);
                elapsed
            })
            .collect();
        samples.sort_unstable();
        let pct_ms = |p: usize| samples[(SAMPLES * p).div_ceil(100) - 1].as_secs_f64() * 1e3;
        self.row(label, samples.len(), Some(pct_ms(50)), pct_ms(95), budget);
    }

    fn rss(&mut self, label: &str, budget: f64) -> Result<(), Box<dyn Error>> {
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
    let mut report = Report::new();
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
    report.latency("Open populated store", None, |_| {
        Store::open(&path).unwrap()
    });
    let store = Store::open(&path)?;
    black_box(store.folder_page(&account.id, INBOX, &top_span)?);
    report.latency(
        "Inbox first page (100 rows)",
        Some(budgets.folder_page_p95_ms),
        |_| store.folder_page(&account.id, INBOX, &top_span).unwrap(),
    );
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
    let mid_page = store.folder_page(&account.id, INBOX, &middle_span)?;
    assert_eq!(mid_page.threads.len(), PAGE_SIZE);
    report.latency(
        "Inbox page near middle cursor (100 rows)",
        Some(budgets.folder_page_p95_ms),
        |_| store.folder_page(&account.id, INBOX, &middle_span).unwrap(),
    );
    report.latency(
        "Read and present a 50-message thread",
        Some(budgets.thread_open_p95_ms),
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
        report.latency(&label, Some(budgets.keyword_search_p95_ms), |_| {
            store.search_with(&account.id, query, options).unwrap()
        });
    }
    let semantic = measure_semantic(&store, &mut report, budgets.semantic_search_p95_ms);
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
        "\n{semantic_note}\n\nFixture generation took {seed_elapsed:.2?} ({disk_mb:.1} MB on disk). Full suite took {:.2?}, including setup and cleanup. Each latency row uses nearest-rank percentiles. RSS values are single process snapshots in decimal MB.",
        suite_started.elapsed()
    )?;
    report.finish()?;
    semantic.map(|_| ())
}

fn measure_semantic(
    store: &Store,
    report: &mut Report,
    budget: f64,
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
    let started = Instant::now();
    let model = OnnxEmbedder::load(&MINILM_L6_V2_INT8, &dir)?;
    mailbox::seed_vectors(store, &model)?;
    store.testing_search_embedder(Box::new(model))?;
    let account = mailbox::account();
    assert!(store.semantic_search_active());
    let warm = store.search(&account.id, HYBRID_QUERIES[0])?;
    assert!(warm.hits.iter().any(|hit| hit.semantic_only));
    let setup = started.elapsed();
    report.latency("Hybrid search (MiniLM + keyword)", Some(budget), |index| {
        let query = HYBRID_QUERIES[index % HYBRID_QUERIES.len()];
        store.search(&account.id, query).unwrap()
    });
    Ok(format!(
        "Hybrid setup took {setup:.2?} and seeded 100k vectors from local MiniLM topic vectors with deterministic jitter."
    ))
}
