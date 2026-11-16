use std::fs;
use std::path::PathBuf;

use crate::RenderMode;
use crate::choose::MailHeaders;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixture {
    pub name: String,
    pub headers: MailHeaders,
    pub html: String,
    pub expected: RenderMode,
}

pub fn fixtures() -> Vec<Fixture> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "html")
                && let Some(name) = path.file_stem().and_then(|stem| stem.to_str())
            {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    names
        .into_iter()
        .filter_map(|name| load(&dir, &name))
        .collect()
}

pub fn fixture(name: &str) -> Option<Fixture> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    load(&dir, name)
}

fn load(dir: &std::path::Path, name: &str) -> Option<Fixture> {
    let html = fs::read_to_string(dir.join(format!("{name}.html"))).ok()?;
    let headers = fs::read_to_string(dir.join(format!("{name}.headers")))
        .map(|text| MailHeaders::from_lines(&text))
        .unwrap_or_default();
    let expected = if name.starts_with("web-") {
        RenderMode::Web
    } else {
        RenderMode::Native
    };
    Some(Fixture {
        name: name.to_string(),
        headers,
        html,
        expected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::choose_view;

    #[test]
    fn corpus_matches_the_filename() {
        let fixtures = fixtures();
        assert!(fixtures.len() >= 8, "fixture corpus is missing files");
        for fixture in fixtures {
            let mode = choose_view(&fixture.headers, &fixture.html);
            assert_eq!(mode, fixture.expected, "{}", fixture.name);
        }
    }
}
