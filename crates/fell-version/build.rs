use std::env;
use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(root)
        .env("TZ", "UTC")
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim_end().to_owned())
}

fn watch_git_path(root: &Path, name: &str, watch_parent: bool) {
    if let Some(path) = git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    ) {
        let path = Path::new(&path);
        // Watch the containing directory when a ref is packed. The next
        // commit can create a loose ref without changing packed-refs.
        let existing = if watch_parent {
            path.ancestors().find(|path| path.exists())
        } else {
            path.exists().then_some(path)
        };
        if let Some(existing) = existing {
            println!("cargo:rerun-if-changed={}", existing.display());
        }
    }
}

fn commit(root: &Path) -> Option<(String, String)> {
    let top = git(root, &["rev-parse", "--show-toplevel"])?;
    if Path::new(&top) != root {
        return None;
    }
    watch_git_path(root, "HEAD", false);
    if let Some(reference) = git(root, &["symbolic-ref", "-q", "HEAD"]) {
        watch_git_path(root, &reference, true);
        watch_git_path(root, "packed-refs", false);
    }

    let revision = git(
        root,
        &[
            "show",
            "-s",
            "--format=%H%n%cd",
            "--date=format-local:%Y-%m-%d",
            "HEAD",
        ],
    )?;
    let (sha, date) = revision.split_once('\n')?;
    Some((sha.get(..12)?.into(), date.into()))
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FELL_RELEASE_VERSION");

    let manifest = env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR");
    let root = Path::new(&manifest).join("../..").canonicalize().unwrap();
    let (sha, date) = commit(&root).unwrap_or_else(|| ("unknown".into(), "unknown".into()));
    let base = env!("CARGO_PKG_VERSION");
    let version = env::var("FELL_RELEASE_VERSION").unwrap_or_else(|_| base.into());
    let rc = version.strip_prefix(&format!("{base}-rc."));
    let numbered_rc = rc.is_some_and(|n| {
        !n.is_empty()
            && (n == "0" || !n.starts_with('0'))
            && n.bytes().all(|ch| ch.is_ascii_digit())
    });
    assert!(
        version == base || version == format!("{base}-rc") || numbered_rc,
        "FELL_RELEASE_VERSION must match {base} or be a release candidate for it"
    );
    println!("cargo:rustc-env=FELL_VERSION=fell {version} ({sha}, {date})");
}
