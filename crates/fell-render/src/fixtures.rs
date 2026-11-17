use crate::RenderMode;
use crate::choose::MailHeaders;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixture {
    pub name: String,
    pub headers: MailHeaders,
    pub html: String,
    pub expected: RenderMode,
}

struct Embedded {
    name: &'static str,
    html: &'static str,
    headers: &'static str,
}

/// Preview mail compiled into the binary. The tarball and AppImage do not
/// ship the source tree, so `CARGO_MANIFEST_DIR` is not a runtime path.
const EMBEDDED: &[Embedded] = &[
    Embedded {
        name: "native-bulk-simple",
        html: include_str!("../fixtures/native-bulk-simple.html"),
        headers: include_str!("../fixtures/native-bulk-simple.headers"),
    },
    Embedded {
        name: "native-esp-simple",
        html: include_str!("../fixtures/native-esp-simple.html"),
        headers: include_str!("../fixtures/native-esp-simple.headers"),
    },
    Embedded {
        name: "native-list-discussion",
        html: include_str!("../fixtures/native-list-discussion.html"),
        headers: include_str!("../fixtures/native-list-discussion.headers"),
    },
    Embedded {
        name: "native-personal-complex",
        html: include_str!("../fixtures/native-personal-complex.html"),
        headers: include_str!("../fixtures/native-personal-complex.headers"),
    },
    Embedded {
        name: "native-personal-reply",
        html: include_str!("../fixtures/native-personal-reply.html"),
        headers: include_str!("../fixtures/native-personal-reply.headers"),
    },
    Embedded {
        name: "native-personal-table",
        html: include_str!("../fixtures/native-personal-table.html"),
        headers: include_str!("../fixtures/native-personal-table.headers"),
    },
    Embedded {
        name: "web-list-digest",
        html: include_str!("../fixtures/web-list-digest.html"),
        headers: include_str!("../fixtures/web-list-digest.headers"),
    },
    Embedded {
        name: "web-newsletter",
        html: include_str!("../fixtures/web-newsletter.html"),
        headers: include_str!("../fixtures/web-newsletter.headers"),
    },
    Embedded {
        name: "web-sendgrid",
        html: include_str!("../fixtures/web-sendgrid.html"),
        headers: include_str!("../fixtures/web-sendgrid.headers"),
    },
];

pub fn fixtures() -> Vec<Fixture> {
    EMBEDDED.iter().map(materialize).collect()
}

pub fn fixture(name: &str) -> Option<Fixture> {
    EMBEDDED
        .iter()
        .find(|embedded| embedded.name == name)
        .map(materialize)
}

fn materialize(embedded: &Embedded) -> Fixture {
    let expected = if embedded.name.starts_with("web-") {
        RenderMode::Web
    } else {
        RenderMode::Native
    };
    Fixture {
        name: embedded.name.to_string(),
        headers: MailHeaders::from_lines(embedded.headers),
        html: embedded.html.to_string(),
        expected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::choose_view;

    #[test]
    fn corpus_matches_the_filename() {
        let fixtures = fixtures();
        assert!(fixtures.len() >= 8, "fixture corpus is missing files");
        assert!(fixture("native-personal-reply").is_some());
        for fixture in fixtures {
            let mode = choose_view(&fixture.headers, &fixture.html);
            assert_eq!(mode, fixture.expected, "{}", fixture.name);
        }
    }
}
