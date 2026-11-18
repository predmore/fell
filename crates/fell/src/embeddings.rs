//! `fell embeddings status|install|remove`: the opt-in for the local
//! embedding model.
//!
//! Fell never fetches model weights on its own: not at first launch, not when
//! it opens the mailbox, not while it indexes. This command is the only place
//! that downloads them. It prints what will be fetched and from where, and
//! asks before it starts. Until then the indexer keeps `local-hash-v1`.

use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fell_core::embed::{
    EMBEDDING_MODEL, EMBEDDING_VERSION, LocalEmbedder, LocalModel, MINILM_L6_V2_INT8, ModelError,
    default_models_dir, install_model, load_local_embedder, model_dir, remove_model,
    verify_installed,
};

const USAGE: &str = "\
fell embeddings status           which embedder indexes mail, and where its files live
fell embeddings install [--yes]  download the pinned all-MiniLM-L6-v2 files, after asking
fell embeddings remove           delete those files; indexing goes back to local-hash-v1

Fell does not download a model unless you run `install`. Until then it
indexes with local-hash-v1, a lexical hash. Search stays keyword search
either way.

  --yes   do not ask first (needed when stdin is not a terminal)

FELL_MODEL_DIR sets where model files go (default $XDG_DATA_HOME/fell/models,
else ~/.local/share/fell/models).";

/// Run `fell embeddings ...`. Returns the process exit code.
pub fn embeddings_command(args: &[String]) -> i32 {
    let stdin = io::stdin();
    let tty = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut out = io::stdout();
    let mut fetch = HttpsFetch::default();
    let result = run(
        args,
        &MINILM_L6_V2_INT8,
        default_models_dir(),
        &mut Console {
            out: &mut out,
            input: &mut input,
            tty,
        },
        &mut |url: &str| fetch.get(url),
    );
    match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("fell: {err}");
            1
        }
    }
}

struct Console<'a> {
    out: &'a mut dyn Write,
    input: &'a mut dyn BufRead,
    tty: bool,
}

type Fetch<'a> = dyn FnMut(&str) -> io::Result<Box<dyn Read>> + 'a;

fn run(
    args: &[String],
    model: &LocalModel,
    root: Option<PathBuf>,
    console: &mut Console<'_>,
    fetch: &mut Fetch<'_>,
) -> Result<(), String> {
    let command = args.first().map(String::as_str);
    if matches!(command, Some("--help" | "-h" | "help") | None) {
        return say(console, USAGE);
    }
    let root =
        root.ok_or("no data directory for models; set HOME, XDG_DATA_HOME, or FELL_MODEL_DIR")?;
    match command {
        Some("status") if args.len() == 1 => status(model, &root, console),
        Some("install") => {
            let yes = match &args[1..] {
                [] => false,
                [flag] if flag == "--yes" || flag == "-y" => true,
                other => return Err(format!("unknown option {:?}\n\n{USAGE}", other[0])),
            };
            install(model, &root, yes, console, fetch)
        }
        Some("remove") if args.len() == 1 => remove(model, &root, console),
        Some(other) => Err(format!("unknown embeddings command {other:?}\n\n{USAGE}")),
        None => unreachable!(),
    }
}

fn say(console: &mut Console<'_>, text: &str) -> Result<(), String> {
    writeln!(console.out, "{text}").map_err(|err| err.to_string())
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

/// Reports what the indexer will do, by loading the model the same way it
/// does. A model is only reported as active when it actually loads.
fn status(model: &LocalModel, root: &Path, console: &mut Console<'_>) -> Result<(), String> {
    let dir = model_dir(model, root);
    let text = match load_local_embedder(model, Some(root)) {
        LocalEmbedder::Model(_) => format!(
            "Indexing with {} version {} ({}).\nFiles: {} (checksums match, the model loads).\nSearch is keyword search; the vectors are not used for ranking yet.",
            model.id.model,
            model.id.version,
            model.title,
            dir.display()
        ),
        LocalEmbedder::Unavailable(err) => format!(
            "Not indexing: the model files at {} are installed but do not load: {err}\nThe index is left as it is, and Fell tries the model again later and each time it opens the mailbox.\nRun `fell embeddings remove` to index with {EMBEDDING_MODEL} instead.",
            dir.display()
        ),
        LocalEmbedder::Placeholder(ModelError::Missing) => format!(
            "Indexing with {EMBEDDING_MODEL} version {EMBEDDING_VERSION}, a lexical hash (no model).\nNo model is installed at {}.\nRun `fell embeddings install` to download {} ({}).",
            dir.display(),
            model.title,
            megabytes(model.download_bytes())
        ),
        LocalEmbedder::Placeholder(err) => format!(
            "Indexing with {EMBEDDING_MODEL} version {EMBEDDING_VERSION}, a lexical hash (no model).\nThe files at {} do not pass their check: {err}\nRun `fell embeddings install` to replace them, or `fell embeddings remove`.",
            dir.display()
        ),
    };
    say(console, &text)
}

/// What `install` will do, shown before it asks.
fn plan(model: &LocalModel, root: &Path) -> String {
    let mut text = format!(
        "Fell will download {}\nfrom Hugging Face, {} in total:\n",
        model.title,
        megabytes(model.download_bytes())
    );
    for file in model.files() {
        text.push_str(&format!(
            "\n  {}\n    {} bytes, SHA-256 {}\n",
            file.url, file.bytes, file.sha256
        ));
    }
    text.push_str(&format!(
        "\nLicense: {} (model card: {})\nSaved to: {} (owner-only)\n\n\
         Each file is checked against the SHA-256 above and deleted if it does not match.\n\
         Once installed, Fell re-indexes mail with this model in the background the next\n\
         time it opens the mailbox. Search stays keyword search.",
        model.license,
        model.card_url,
        model_dir(model, root).display()
    ));
    text
}

fn install(
    model: &LocalModel,
    root: &Path,
    yes: bool,
    console: &mut Console<'_>,
    fetch: &mut Fetch<'_>,
) -> Result<(), String> {
    if let Ok(dir) = verify_installed(model, root) {
        return say(
            console,
            &format!(
                "{} is already installed at {} (checksums match).",
                model.id.model,
                dir.display()
            ),
        );
    }
    say(console, &plan(model, root))?;
    if !yes {
        if !console.tty {
            return Err(
                "not downloading: stdin is not a terminal. Read the above and rerun with --yes to agree."
                    .into(),
            );
        }
        write!(
            console.out,
            "\nDownload {} now? [y/N] ",
            megabytes(model.download_bytes())
        )
        .and_then(|()| console.out.flush())
        .map_err(|err| err.to_string())?;
        let mut answer = String::new();
        console
            .input
            .read_line(&mut answer)
            .map_err(|err| err.to_string())?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            return say(console, "Nothing downloaded. Fell keeps local-hash-v1.");
        }
    }
    let mut announce = |url: &str| {
        let _ = writeln!(console.out, "Downloading {url}");
        fetch(url)
    };
    let dir = install_model(model, root, &mut announce)
        .map_err(|err| format!("{err}. Nothing was installed."))?;
    say(
        console,
        &format!(
            "Installed {} at {}.\nFell re-indexes mail with it in the background the next time it opens the mailbox.",
            model.id.model,
            dir.display()
        ),
    )
}

fn remove(model: &LocalModel, root: &Path, console: &mut Console<'_>) -> Result<(), String> {
    let dir = model_dir(model, root);
    if remove_model(model, root).map_err(|err| format!("{}: {err}", dir.display()))? {
        say(
            console,
            &format!(
                "Removed {}.\nFell goes back to {EMBEDDING_MODEL} the next time it opens the mailbox.",
                dir.display()
            ),
        )
    } else {
        say(console, &format!("Nothing to remove at {}.", dir.display()))
    }
}

/// HTTPS GET with the system's trust roots. Built on first use, which is
/// after the person agreed to the download.
#[derive(Default)]
struct HttpsFetch {
    agent: Option<ureq::Agent>,
}

impl HttpsFetch {
    fn get(&mut self, url: &str) -> io::Result<Box<dyn Read>> {
        let agent = match &self.agent {
            Some(agent) => agent,
            None => self.agent.insert(https_agent()?),
        };
        let response = agent.get(url).call().map_err(io::Error::other)?;
        Ok(Box::new(response.into_body().into_reader()))
    }
}

/// No single download may take longer than this, however slowly the bytes
/// keep arriving. 23 MB in five minutes is about 80 KB/s.
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(5 * 60);

fn https_agent() -> io::Result<ureq::Agent> {
    use ureq::tls::{Certificate, RootCerts, TlsConfig, TlsProvider};
    let roots: Vec<Certificate<'static>> = rustls_native_certs::load_native_certs()
        .certs
        .iter()
        .map(|cert| Certificate::from_der(cert.as_ref()).to_owned())
        .collect();
    if roots.is_empty() {
        return Err(io::Error::other(
            "no TLS trust anchors in the system certificate store",
        ));
    }
    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .root_certs(RootCerts::new_with_certs(&roots))
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    Ok(ureq::Agent::config_builder()
        // Hugging Face answers with a redirect to its CDN. Both are HTTPS.
        .https_only(true)
        .max_redirects(5)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .timeout_per_read(Some(Duration::from_secs(60)))
        .timeout_recv_body(Some(DOWNLOAD_DEADLINE))
        .timeout_global(Some(DOWNLOAD_DEADLINE))
        .user_agent(concat!("fell/", env!("CARGO_PKG_VERSION")))
        .tls_config(tls)
        .build()
        .new_agent())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "fell-embeddings-cli-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    /// Runs one command with `answer` on stdin. Returns the result, stdout,
    /// and how many times the fetcher was called.
    fn invoke(
        list: &[&str],
        root: &Path,
        tty: bool,
        answer: &str,
        serve: impl Fn(&str) -> Vec<u8>,
    ) -> (Result<(), String>, String, usize) {
        invoke_model(list, &MINILM_L6_V2_INT8, root, tty, answer, serve)
    }

    fn invoke_model(
        list: &[&str],
        model: &LocalModel,
        root: &Path,
        tty: bool,
        answer: &str,
        serve: impl Fn(&str) -> Vec<u8>,
    ) -> (Result<(), String>, String, usize) {
        let calls = Cell::new(0);
        let mut out = Vec::new();
        let mut input = io::Cursor::new(answer.as_bytes().to_vec());
        let result = run(
            &args(list),
            model,
            Some(root.to_path_buf()),
            &mut Console {
                out: &mut out,
                input: &mut input,
                tty,
            },
            &mut |url: &str| {
                calls.set(calls.get() + 1);
                Ok(Box::new(io::Cursor::new(serve(url))) as Box<dyn Read>)
            },
        );
        (result, String::from_utf8(out).unwrap(), calls.get())
    }

    fn never(_: &str) -> Vec<u8> {
        panic!("this command must not fetch");
    }

    #[test]
    fn status_reports_the_placeholder_and_does_not_fetch_or_create() {
        let root = temp_root("status");
        let (result, out, calls) = invoke(&["status"], &root, true, "", never);
        result.unwrap();
        assert_eq!(calls, 0);
        assert!(out.contains("local-hash-v1"), "{out}");
        assert!(out.contains("fell embeddings install"), "{out}");
        assert!(!root.exists());
    }

    #[test]
    fn install_shows_what_and_where_before_asking() {
        let root = temp_root("plan");
        let (result, out, calls) = invoke(&["install"], &root, true, "n\n", never);
        result.unwrap();
        assert_eq!(calls, 0);
        let model = MINILM_L6_V2_INT8;
        for file in model.files() {
            assert!(out.contains(file.url), "{out}");
            assert!(out.contains(file.sha256), "{out}");
        }
        assert!(out.contains("Apache-2.0"), "{out}");
        assert!(out.contains(model.card_url), "{out}");
        assert!(
            out.contains(&model_dir(&model, &root).display().to_string()),
            "{out}"
        );
        assert!(out.contains("[y/N]"), "{out}");
        assert!(out.contains("Nothing downloaded"), "{out}");
        assert!(!root.exists(), "declining creates nothing");
    }

    #[test]
    fn install_without_a_terminal_needs_yes() {
        let root = temp_root("notty");
        // Even a "y" on piped stdin is not consent; only --yes is.
        let (result, out, calls) = invoke(&["install"], &root, false, "y\n", never);
        assert!(result.unwrap_err().contains("--yes"));
        assert_eq!(calls, 0);
        assert!(out.contains(MINILM_L6_V2_INT8.onnx.url));
        assert!(!root.exists());
    }

    #[test]
    fn a_confirmed_download_with_the_wrong_bytes_is_rejected() {
        let root = temp_root("checksum");
        let model = MINILM_L6_V2_INT8;
        let (result, out, calls) = invoke(&["install"], &root, true, "y\n", |url| {
            // Right length, wrong content.
            let len = if url == model.onnx.url {
                model.onnx.bytes
            } else {
                model.vocab.bytes
            };
            vec![0u8; len as usize]
        });
        let err = result.unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
        assert!(err.contains("Nothing was installed"), "{err}");
        assert_eq!(calls, 1, "stops at the first bad file");
        assert!(out.contains("Downloading https://huggingface.co/"), "{out}");
        assert!(fell_core::embed::installed_model_dir(&model, &root).is_none());
        let (_, status, _) = invoke(&["status"], &root, true, "", never);
        assert!(status.contains("No model is installed"), "{status}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn yes_skips_the_question_but_not_the_checksum() {
        let root = temp_root("yes");
        let (result, out, calls) = invoke(&["install", "--yes"], &root, false, "", |_| {
            b"short".to_vec()
        });
        assert!(result.unwrap_err().contains("bytes"));
        assert_eq!(calls, 1);
        assert!(!out.contains("[y/N]"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_without_a_model_is_a_no_op_and_bad_commands_fail() {
        let root = temp_root("remove");
        let (result, out, calls) = invoke(&["remove"], &root, true, "", never);
        result.unwrap();
        assert_eq!(calls, 0);
        assert!(out.contains("Nothing to remove"), "{out}");
        assert!(invoke(&["fetch"], &root, true, "", never).0.is_err());
        assert!(
            invoke(&["install", "--force"], &root, true, "", never)
                .0
                .is_err()
        );
        let (help, text, _) = invoke(&[], &root, true, "", never);
        help.unwrap();
        assert!(text.contains("fell embeddings install"));
    }

    /// A model pinned to `onnx` and `vocab`, so those bytes pass the check.
    fn pinned_to(onnx: &'static [u8], vocab: &'static [u8]) -> &'static LocalModel {
        use sha2::{Digest, Sha256};
        let hex = |bytes: &[u8]| -> &'static str {
            let text: String = Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            Box::leak(text.into_boxed_str())
        };
        let mut model = MINILM_L6_V2_INT8;
        model.dir_name = "test-model";
        model.onnx.url = "https://example.invalid/model.onnx";
        model.onnx.sha256 = hex(onnx);
        model.onnx.bytes = onnx.len() as u64;
        model.vocab.url = "https://example.invalid/vocab.txt";
        model.vocab.sha256 = hex(vocab);
        model.vocab.bytes = vocab.len() as u64;
        Box::leak(Box::new(model))
    }

    #[test]
    fn status_does_not_report_a_model_that_will_not_load() {
        let root = temp_root("unloadable");
        let onnx: &[u8] = b"verified bytes, but not an ONNX graph";
        let vocab: &[u8] = b"[PAD]\n[UNK]\n[CLS]\n[SEP]\n";
        let model = pinned_to(onnx, vocab);
        let (result, _, calls) =
            invoke_model(&["install", "--yes"], model, &root, false, "", |url| {
                if url == model.onnx.url {
                    onnx.to_vec()
                } else {
                    vocab.to_vec()
                }
            });
        result.unwrap();
        assert_eq!(calls, 2);
        verify_installed(model, &root).unwrap();
        let (result, out, calls) = invoke_model(&["status"], model, &root, true, "", never);
        result.unwrap();
        assert_eq!(calls, 0);
        assert!(!out.contains("Indexing with"), "{out}");
        assert!(out.contains("do not load"), "{out}");
        assert!(out.contains("left as it is"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_download_has_a_total_deadline() {
        let agent = https_agent().unwrap();
        let timeouts = agent.config().timeouts();
        assert_eq!(timeouts.global, Some(DOWNLOAD_DEADLINE));
        assert_eq!(timeouts.recv_body, Some(DOWNLOAD_DEADLINE));
        assert!(DOWNLOAD_DEADLINE <= Duration::from_secs(10 * 60));
    }
}
