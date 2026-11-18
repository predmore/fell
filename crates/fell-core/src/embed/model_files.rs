//! Pinned weight files for the local embedding model, and the opt-in install.
//!
//! Nothing here runs on its own. [`install_model`] is the only function that
//! fetches, and it takes the fetcher as an argument: this crate links no HTTP
//! client. The `fell embeddings install` command passes one after the person
//! has agreed to the download. Opening a mailbox and indexing only look for
//! files that are already on disk (see [`installed_model_dir`]).
//!
//! Each file is pinned to a Hugging Face commit, a byte size and a SHA-256.
//! A download that differs in either is deleted, and so is a file on disk that
//! no longer matches when the indexer loads it.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::EmbedderId;

/// One file the model needs, pinned by size and SHA-256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFile {
    /// File name inside the model directory.
    pub name: &'static str,
    /// Where `fell embeddings install` fetches it. Always HTTPS, always a
    /// fixed commit, never a branch.
    pub url: &'static str,
    /// Lowercase hex SHA-256 of the exact bytes.
    pub sha256: &'static str,
    pub bytes: u64,
}

/// A sentence-embedding model that can run locally once its files are present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalModel {
    /// Stored on each vector. A new id or version rewrites the index.
    pub id: EmbedderId,
    /// Directory under the models root.
    pub dir_name: &'static str,
    /// Human-readable name for the install prompt.
    pub title: &'static str,
    /// Model card the license was read from.
    pub card_url: &'static str,
    /// SPDX id of the weights' license.
    pub license: &'static str,
    /// ONNX graph with weights, outputs `last_hidden_state`.
    pub onnx: ModelFile,
    /// WordPiece vocabulary, one token per line.
    pub vocab: ModelFile,
    /// Word pieces per message, markers included. Longer bodies are truncated.
    pub max_tokens: usize,
}

/// `sentence-transformers/all-MiniLM-L6-v2`, the int8 ONNX export.
///
/// License: Apache-2.0, read from the model card at the pinned commit
/// (`license: apache-2.0` in the README front matter), which covers the ONNX
/// files in the same repository. The base model
/// `nreimers/MiniLM-L6-H384-uncased` is MIT.
///
/// Source: <https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/blob/1110a243fdf4706b3f48f1d95db1a4f5529b4d41/onnx/model_qint8_arm64.onnx>.
/// The repository publishes the same int8 bytes as `model_qint8_arm64.onnx`,
/// `model_qint8_avx512.onnx` and `model_qint8_avx512_vnni.onnx` (one SHA-256
/// for all three). The graph uses portable ONNX ops (`DynamicQuantizeLinear`,
/// `MatMulInteger`), so tract runs it on aarch64 and x86_64 alike.
///
/// 384 dimensions, mean pooling over tokens, then L2 normalisation, as the
/// repository's `modules.json` and `1_Pooling/config.json` describe. The card's
/// `max_seq_length` is 256.
pub const MINILM_L6_V2_INT8: LocalModel = LocalModel {
    id: EmbedderId {
        model: "all-MiniLM-L6-v2-qint8",
        // Bump when the pinned files, the tokenizer, truncation, or pooling
        // change. Rows with another version are rewritten.
        version: "1",
    },
    dir_name: "all-MiniLM-L6-v2-qint8",
    title: "all-MiniLM-L6-v2 sentence embeddings (int8 ONNX, 384 dimensions)",
    card_url: "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2",
    license: "Apache-2.0",
    onnx: ModelFile {
        name: "model.onnx",
        url: concat!(
            "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/",
            "1110a243fdf4706b3f48f1d95db1a4f5529b4d41",
            "/onnx/model_qint8_arm64.onnx"
        ),
        sha256: "4278337fd0ff3c68bfb6291042cad8ab363e1d9fbc43dcb499fe91c871902474",
        bytes: 23_026_053,
    },
    vocab: ModelFile {
        name: "vocab.txt",
        url: concat!(
            "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/",
            "1110a243fdf4706b3f48f1d95db1a4f5529b4d41",
            "/vocab.txt"
        ),
        sha256: "07eced375cec144d27c900241f3e339478dec958f92fddbc551f295c992038a3",
        bytes: 231_508,
    },
    max_tokens: 256,
};

impl LocalModel {
    pub fn files(&self) -> [&ModelFile; 2] {
        [&self.onnx, &self.vocab]
    }

    pub fn download_bytes(&self) -> u64 {
        self.onnx.bytes + self.vocab.bytes
    }
}

/// Download, checksum, or load failure.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("{file}: SHA-256 {actual} does not match the pinned {expected}")]
    Checksum {
        file: &'static str,
        expected: &'static str,
        actual: String,
    },
    #[error("{file}: expected {expected} bytes, got {actual}")]
    Size {
        file: &'static str,
        expected: u64,
        /// What was read. A file larger than expected stops at one extra byte.
        actual: u64,
    },
    #[error("{file}: {source}")]
    Fetch {
        file: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("model files are not installed")]
    Missing,
    #[error("cannot load the model: {0}")]
    Load(String),
}

/// Root that holds downloaded models: `$FELL_MODEL_DIR`, else
/// `$XDG_DATA_HOME/fell/models`, else `~/.local/share/fell/models`.
///
/// Inside this crate's own tests it is `None` unless a test sets one, so a
/// model on the developer's machine cannot change test results.
pub fn default_models_dir() -> Option<PathBuf> {
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        models_dir_from(
            std::env::var_os("FELL_MODEL_DIR").as_deref(),
            std::env::var_os("XDG_DATA_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    }
}

#[cfg_attr(test, allow(dead_code))]
fn models_dir_from(
    explicit: Option<&std::ffi::OsStr>,
    xdg_data: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(dir) = explicit.filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if let Some(data) = xdg_data.filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(data).join("fell/models"));
    }
    home.filter(|dir| !dir.is_empty())
        .map(|home| PathBuf::from(home).join(".local/share/fell/models"))
}

/// Where `model` lives under `root`, installed or not.
pub fn model_dir(model: &LocalModel, root: &Path) -> PathBuf {
    root.join(model.dir_name)
}

/// The model directory when every file is present as a regular file.
///
/// This only stats the files. It does not hash them, read the network, or
/// create anything. The indexer checks the hashes when it loads the files.
pub fn installed_model_dir(model: &LocalModel, root: &Path) -> Option<PathBuf> {
    let dir = model_dir(model, root);
    model
        .files()
        .iter()
        .all(|file| {
            fs::symlink_metadata(dir.join(file.name)).is_ok_and(|meta| meta.file_type().is_file())
        })
        .then_some(dir)
}

/// Read one installed file and check it against its pin.
pub(crate) fn read_verified(dir: &Path, file: &ModelFile) -> Result<Vec<u8>, ModelError> {
    let path = dir.join(file.name);
    let handle = fs::File::open(&path)?;
    let mut bytes = Vec::with_capacity(usize::try_from(file.bytes).unwrap_or(0));
    handle
        .take(file.bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    check(file, bytes.len() as u64, &Sha256::digest(&bytes))?;
    Ok(bytes)
}

/// Hash every installed file. `Err(Missing)` when one is absent.
pub fn verify_installed(model: &LocalModel, root: &Path) -> Result<PathBuf, ModelError> {
    let dir = installed_model_dir(model, root).ok_or(ModelError::Missing)?;
    for file in model.files() {
        read_verified(&dir, file)?;
    }
    Ok(dir)
}

fn check(file: &ModelFile, len: u64, digest: &[u8]) -> Result<(), ModelError> {
    if len != file.bytes {
        return Err(ModelError::Size {
            file: file.name,
            expected: file.bytes,
            actual: len,
        });
    }
    let actual = hex(digest);
    if actual != file.sha256 {
        return Err(ModelError::Checksum {
            file: file.name,
            expected: file.sha256,
            actual,
        });
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Opens a byte stream for one URL. The caller decides how, and whether.
pub type Fetch<'a> = dyn FnMut(&str) -> io::Result<Box<dyn Read>> + 'a;

/// Download `model` into `root` and check every file.
///
/// Call this only after the person agreed to fetch [`LocalModel::files`] from
/// those URLs. Files go to an owner-only staging directory first and are
/// renamed into place only when every size and SHA-256 matches, so a failed or
/// tampered download leaves no model behind. An install that already verifies
/// is kept and nothing is fetched.
pub fn install_model(
    model: &LocalModel,
    root: &Path,
    fetch: &mut Fetch<'_>,
) -> Result<PathBuf, ModelError> {
    if let Ok(dir) = verify_installed(model, root) {
        return Ok(dir);
    }
    create_private_dirs(root)?;
    let staging = root.join(format!(
        ".{}.partial-{}-{}",
        model.dir_name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|time| time.as_nanos())
            .unwrap_or(0)
    ));
    create_private_dir(&staging)?;
    let staged = (|| {
        for file in model.files() {
            download_one(file, &staging, fetch)?;
        }
        Ok::<_, ModelError>(())
    })();
    if let Err(err) = staged {
        let _ = fs::remove_dir_all(&staging);
        return Err(err);
    }
    let dir = model_dir(model, root);
    if fs::symlink_metadata(&dir).is_ok() {
        // A partial or stale install that failed verification above.
        if let Err(err) = remove_path(&dir) {
            let _ = fs::remove_dir_all(&staging);
            return Err(err.into());
        }
    }
    if let Err(err) = fs::rename(&staging, &dir) {
        let _ = fs::remove_dir_all(&staging);
        return Err(err.into());
    }
    if let Ok(handle) = fs::File::open(root) {
        let _ = handle.sync_all();
    }
    Ok(dir)
}

fn download_one(file: &ModelFile, staging: &Path, fetch: &mut Fetch<'_>) -> Result<(), ModelError> {
    let reader = fetch(file.url).map_err(|source| ModelError::Fetch {
        file: file.name,
        source,
    })?;
    let path = staging.join(file.name);
    let mut out = create_private_file(&path)?;
    let mut hasher = Sha256::new();
    let mut limited = reader.take(file.bytes.saturating_add(1));
    let mut buf = vec![0u8; 64 * 1024];
    let mut len = 0u64;
    loop {
        let n = match limited.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(source) => {
                return Err(ModelError::Fetch {
                    file: file.name,
                    source,
                });
            }
        };
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        len += n as u64;
    }
    check(file, len, &hasher.finalize())?;
    out.sync_all()?;
    Ok(())
}

/// Delete the downloaded model. `Ok(false)` when it was not there.
pub fn remove_model(model: &LocalModel, root: &Path) -> io::Result<bool> {
    let dir = model_dir(model, root);
    match fs::symlink_metadata(&dir) {
        Ok(_) => {
            remove_path(&dir)?;
            Ok(true)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

fn remove_path(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Create `dir` and any missing parents, each owner-only. Existing parents keep
/// their mode. `dir` itself is set to 0700 either way.
fn create_private_dirs(dir: &Path) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut current = dir;
    while fs::symlink_metadata(current).is_err() {
        missing.push(current.to_path_buf());
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => current = parent,
            _ => break,
        }
    }
    for path in missing.iter().rev() {
        create_private_dir(path)?;
    }
    restrict(dir, 0o700)
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => restrict(path, 0o700),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

fn create_private_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn restrict(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A [`LocalModel`] whose pins match `onnx` and `vocab`. Leaks a few
    /// strings, which is fine in a test process.
    pub(crate) fn model_for(
        id: EmbedderId,
        onnx: &[u8],
        vocab: &[u8],
        max_tokens: usize,
    ) -> &'static LocalModel {
        let leak = |text: String| -> &'static str { Box::leak(text.into_boxed_str()) };
        Box::leak(Box::new(LocalModel {
            id,
            dir_name: "test-model",
            title: "test model",
            card_url: "https://example.invalid/card",
            license: "MIT",
            onnx: ModelFile {
                name: "model.onnx",
                url: "https://example.invalid/model.onnx",
                sha256: leak(hex(&Sha256::digest(onnx))),
                bytes: onnx.len() as u64,
            },
            vocab: ModelFile {
                name: "vocab.txt",
                url: "https://example.invalid/vocab.txt",
                sha256: leak(hex(&Sha256::digest(vocab))),
                bytes: vocab.len() as u64,
            },
            max_tokens,
        }))
    }

    /// A fetcher that serves fixed bytes per URL and counts calls.
    pub(crate) fn serve<'a>(
        files: Vec<(&'static str, Vec<u8>)>,
        calls: &'a std::cell::Cell<usize>,
    ) -> impl FnMut(&str) -> io::Result<Box<dyn Read>> + 'a {
        move |url| {
            calls.set(calls.get() + 1);
            files
                .iter()
                .find(|(want, _)| *want == url)
                .map(|(_, bytes)| Box::new(io::Cursor::new(bytes.clone())) as Box<dyn Read>)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, url.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{model_for, serve};
    use super::*;
    use std::cell::Cell;

    /// Hugging Face commit every [`MINILM_L6_V2_INT8`] URL is pinned to.
    const MINILM_REVISION: &str = "1110a243fdf4706b3f48f1d95db1a4f5529b4d41";

    const ID: EmbedderId = EmbedderId {
        model: "test-files",
        version: "1",
    };

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fell-model-files-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn pinned_urls_are_https_at_a_fixed_commit() {
        let model = MINILM_L6_V2_INT8;
        for file in model.files() {
            assert!(
                file.url.starts_with("https://huggingface.co/"),
                "{}",
                file.url
            );
            assert!(file.url.contains(MINILM_REVISION), "{}", file.url);
            assert_eq!(file.sha256.len(), 64);
            assert!(
                file.sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            );
        }
        assert_eq!(model.license, "Apache-2.0");
        assert_ne!(model.id.model, super::super::EMBEDDING_MODEL);
    }

    #[test]
    fn models_dir_prefers_the_override_then_xdg_then_home() {
        use std::ffi::OsStr;
        assert_eq!(
            models_dir_from(
                Some(OsStr::new("/m")),
                Some(OsStr::new("/x")),
                Some(OsStr::new("/h"))
            ),
            Some(PathBuf::from("/m"))
        );
        assert_eq!(
            models_dir_from(None, Some(OsStr::new("/x")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/x/fell/models"))
        );
        assert_eq!(
            models_dir_from(Some(OsStr::new("")), None, Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.local/share/fell/models"))
        );
        assert_eq!(models_dir_from(None, None, None), None);
        assert_eq!(
            default_models_dir(),
            None,
            "tests never see a real model dir"
        );
    }

    #[test]
    fn install_writes_owner_only_files_and_skips_a_verified_copy() {
        let root = temp_root("install");
        let model = model_for(ID, b"onnx bytes", b"[CLS]\n[SEP]\n[UNK]\n", 8);
        let calls = Cell::new(0);
        let mut fetch = serve(
            vec![
                (model.onnx.url, b"onnx bytes".to_vec()),
                (model.vocab.url, b"[CLS]\n[SEP]\n[UNK]\n".to_vec()),
            ],
            &calls,
        );
        assert!(installed_model_dir(model, &root).is_none());
        let dir = install_model(model, &root, &mut fetch).unwrap();
        assert_eq!(calls.get(), 2);
        assert_eq!(installed_model_dir(model, &root), Some(dir.clone()));
        assert_eq!(fs::read(dir.join("model.onnx")).unwrap(), b"onnx bytes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&root), 0o700);
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&dir.join("model.onnx")), 0o600);
            assert_eq!(mode(&dir.join("vocab.txt")), 0o600);
        }
        install_model(model, &root, &mut fetch).unwrap();
        assert_eq!(calls.get(), 2, "a verified install is not fetched again");
        assert!(remove_model(model, &root).unwrap());
        assert!(!remove_model(model, &root).unwrap());
        assert!(installed_model_dir(model, &root).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_checksum_mismatch_keeps_nothing() {
        let root = temp_root("checksum");
        let model = model_for(ID, b"onnx bytes", b"vocab", 8);
        let calls = Cell::new(0);
        // Same length, different bytes.
        let mut fetch = serve(
            vec![
                (model.onnx.url, b"onnx BYTES".to_vec()),
                (model.vocab.url, b"vocab".to_vec()),
            ],
            &calls,
        );
        let err = install_model(model, &root, &mut fetch).unwrap_err();
        assert!(
            matches!(
                err,
                ModelError::Checksum {
                    file: "model.onnx",
                    ..
                }
            ),
            "{err}"
        );
        assert!(installed_model_dir(model, &root).is_none());
        assert_eq!(
            fs::read_dir(&root).unwrap().count(),
            0,
            "staging is removed"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_oversized_or_short_download_is_rejected() {
        let root = temp_root("size");
        let model = model_for(ID, b"onnx", b"vocab", 8);
        let calls = Cell::new(0);
        let mut long = serve(
            vec![
                (model.onnx.url, vec![b'x'; 1 << 20]),
                (model.vocab.url, b"vocab".to_vec()),
            ],
            &calls,
        );
        let err = install_model(model, &root, &mut long).unwrap_err();
        assert!(
            matches!(
                err,
                ModelError::Size {
                    expected: 4,
                    actual: 5,
                    ..
                }
            ),
            "{err}"
        );
        let mut short = serve(
            vec![
                (model.onnx.url, b"onn".to_vec()),
                (model.vocab.url, b"vocab".to_vec()),
            ],
            &calls,
        );
        assert!(matches!(
            install_model(model, &root, &mut short).unwrap_err(),
            ModelError::Size { actual: 3, .. }
        ));
        assert!(installed_model_dir(model, &root).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_fetch_keeps_nothing() {
        let root = temp_root("fetch");
        let model = model_for(ID, b"onnx", b"vocab", 8);
        let calls = Cell::new(0);
        let mut fetch = serve(vec![(model.onnx.url, b"onnx".to_vec())], &calls);
        let err = install_model(model, &root, &mut fetch).unwrap_err();
        assert!(
            matches!(
                err,
                ModelError::Fetch {
                    file: "vocab.txt",
                    ..
                }
            ),
            "{err}"
        );
        assert!(installed_model_dir(model, &root).is_none());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_tampered_install_fails_verification_and_is_replaced() {
        let root = temp_root("tamper");
        let model = model_for(ID, b"onnx", b"vocab", 8);
        let calls = Cell::new(0);
        let mut fetch = serve(
            vec![
                (model.onnx.url, b"onnx".to_vec()),
                (model.vocab.url, b"vocab".to_vec()),
            ],
            &calls,
        );
        let dir = install_model(model, &root, &mut fetch).unwrap();
        fs::write(dir.join("model.onnx"), b"evil").unwrap();
        assert!(matches!(
            verify_installed(model, &root).unwrap_err(),
            ModelError::Checksum {
                file: "model.onnx",
                ..
            }
        ));
        assert!(read_verified(&dir, &model.onnx).is_err());
        install_model(model, &root, &mut fetch).unwrap();
        assert_eq!(calls.get(), 4);
        verify_installed(model, &root).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symlinked_file_is_not_an_install() {
        let root = temp_root("symlink");
        let model = model_for(ID, b"onnx", b"vocab", 8);
        let dir = model_dir(model, &root);
        fs::create_dir_all(&dir).unwrap();
        fs::write(root.join("elsewhere"), b"onnx").unwrap();
        fs::write(dir.join("vocab.txt"), b"vocab").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("elsewhere"), dir.join("model.onnx")).unwrap();
            assert!(installed_model_dir(model, &root).is_none());
        }
        let _ = fs::remove_dir_all(&root);
    }
}
