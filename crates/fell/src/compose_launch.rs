//! Inputs for opening a personal draft from outside the editor.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read as _};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use fell_core::compose::{
    DraftFields, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENT_FILES, ResolvedFile, parse_address_line,
    plain_text_to_markdown,
};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ComposeInput {
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: String,
    body: String,
}

impl ComposeInput {
    pub(crate) fn check_headers(&self) -> Result<(), String> {
        check_header(&self.subject)?;
        for address in self.to.iter().chain(&self.cc).chain(&self.bcc) {
            check_header(address)?;
        }
        Ok(())
    }

    pub(crate) fn into_fields_and_body(self, from: String) -> (DraftFields, String) {
        (
            DraftFields {
                from,
                to: self.to,
                cc: self.cc,
                bcc: self.bcc,
                subject: self.subject,
                ..DraftFields::default()
            },
            self.body,
        )
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ComposeLaunch {
    pub(crate) input: ComposeInput,
    pub(crate) files: Vec<ResolvedFile>,
}

fn check_header(value: &str) -> Result<(), String> {
    if value.contains(['\u{2028}', '\u{2029}']) || value.chars().any(char::is_control) {
        return Err("compose fields must not contain control characters or line breaks".into());
    }
    Ok(())
}

fn decode(raw: &str) -> Result<String, String> {
    let bytes = raw.as_bytes();
    if raw.match_indices('%').any(|(at, _)| {
        !matches!(&bytes[at..], [b'%', a, b, ..] if a.is_ascii_hexdigit() && b.is_ascii_hexdigit())
    }) {
        return Err("invalid percent escape in mailto link".into());
    }
    let decoded = percent_decode_str(raw)
        .decode_utf8()
        .map_err(|_| "mailto fields must be UTF-8")?;
    check_header(&decoded)?;
    Ok(decoded.into_owned())
}

pub fn is_mailto(arg: &str) -> bool {
    arg.split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("mailto"))
}

pub fn mailto(uri: &str) -> Result<ComposeLaunch, String> {
    if !is_mailto(uri) || uri.contains('#') {
        return Err("invalid mailto link".into());
    }
    let (_, rest) = uri.split_once(':').expect("mailto scheme");
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut input = ComposeInput::default();
    if !path.is_empty() {
        for raw in path.split(',') {
            let address = decode(raw)?;
            let parsed = mailto_addresses(&address)?;
            if parsed.len() != 1 {
                return Err("invalid recipient in mailto path".into());
            }
            input.to.extend(parsed);
        }
    }

    let mut subject = None;
    let mut body = None;
    for field in query.split('&').filter(|field| !field.is_empty()) {
        let (name, value) = field
            .split_once('=')
            .ok_or("mailto fields must have a value after '='")?;
        let name = decode(name)?.to_ascii_lowercase();
        let value = decode(value)?;
        let slot = match name.as_str() {
            "to" => &mut input.to,
            "cc" => &mut input.cc,
            "bcc" => &mut input.bcc,
            "subject" => {
                set_mailto_field(&mut subject, value)?;
                continue;
            }
            "body" => {
                set_mailto_field(&mut body, value)?;
                continue;
            }
            _ => continue,
        };
        slot.extend(mailto_addresses(&value)?);
    }
    input.subject = subject.unwrap_or_default();
    input.body = body.map_or_else(String::new, |body| plain_text_to_markdown(&body));
    Ok(ComposeLaunch {
        input,
        files: Vec::new(),
    })
}

fn mailto_addresses(value: &str) -> Result<Vec<String>, String> {
    let mut addresses = parse_address_line(value)?;
    for address in &mut addresses {
        let (local, domain) = address
            .rsplit_once('@')
            .ok_or("invalid recipient in mailto link")?;
        let local_is_quoted = local.starts_with('"') && local.ends_with('"');
        let domain_literal = domain.starts_with('[') && domain.ends_with(']');
        if local.is_empty()
            || (!local_is_quoted && local.contains(['@', '"', '\\', '(', ')', ':', ';', '[', ']']))
            || domain.is_empty()
            || address.contains(['<', '>'])
            || domain.chars().any(|ch| {
                ch.is_ascii()
                    && !(ch.is_ascii_alphanumeric()
                        || matches!(ch, '.' | '-' | '_')
                        || (domain_literal && matches!(ch, '[' | ']' | ':')))
            })
        {
            return Err("invalid recipient in mailto link".into());
        }
        if !domain.is_ascii() {
            let url::Host::Domain(ascii) = url::Host::parse(domain)
                .map_err(|_| "invalid international domain in mailto link")?
            else {
                return Err("invalid international domain in mailto link".into());
            };
            *address = format!("{local}@{ascii}");
        }
    }
    Ok(addresses)
}

fn set_mailto_field(slot: &mut Option<String>, value: String) -> Result<(), String> {
    if slot.replace(value).is_some() {
        return Err("mailto link repeats its subject or body field".into());
    }
    Ok(())
}

/// Parse CLI options and resolve attachment paths in the invoking process.
/// A path can be a non-UTF-8 Unix filename.
pub fn cli(args: &[OsString]) -> Result<ComposeLaunch, String> {
    let mut launch = ComposeLaunch::default();
    let mut paths = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut parts = arg.as_bytes().splitn(2, |&byte| byte == b'=');
        let flag = std::str::from_utf8(parts.next().expect("split contains the option"))
            .map_err(|_| "compose option must be UTF-8")?;
        let inline = parts.next().map(OsStr::from_bytes);
        match flag {
            "--to" | "--cc" | "--bcc" | "--subject" | "--body" | "--attach" => {}
            _ => return Err(format!("unknown compose option {flag:?}")),
        }
        let value = inline
            .or_else(|| {
                args.next()
                    .filter(|arg| !arg.as_bytes().starts_with(b"--"))
                    .map(OsString::as_os_str)
            })
            .ok_or_else(|| format!("{flag} needs a value"))?;
        if flag == "--attach" {
            paths.push(Path::new(value));
            continue;
        }
        let value = value
            .to_str()
            .ok_or_else(|| format!("{flag} must be UTF-8"))?;
        if flag == "--body" {
            launch.input.body = value.replace("\r\n", "\n");
            continue;
        }
        check_header(value)?;
        match flag {
            "--to" => launch.input.to.extend(parse_address_line(value)?),
            "--cc" => launch.input.cc.extend(parse_address_line(value)?),
            "--bcc" => launch.input.bcc.extend(parse_address_line(value)?),
            "--subject" => launch.input.subject = value.into(),
            _ => unreachable!(),
        }
    }

    if paths.len() > MAX_ATTACHMENT_FILES {
        return Err(format!("compose allows {MAX_ATTACHMENT_FILES} attachments"));
    }
    let mut remaining = MAX_ATTACHMENT_BYTES;
    for (index, path) in paths.into_iter().enumerate() {
        let bytes = read_attachment(path, remaining)
            .map_err(|err| format!("cannot attach {:?}: {err}", path.as_os_str()))?;
        remaining -= bytes.len() as u64;
        let name = path.file_name().map(|name| name.to_string_lossy());
        launch.files.push(attachment(index, name.as_deref(), bytes));
    }
    Ok(launch)
}

pub(crate) fn attachment(index: usize, name: Option<&str>, bytes: Vec<u8>) -> ResolvedFile {
    let name = fell_core::parts::sanitize_filename(name, "");
    ResolvedFile {
        path: format!("attachments/{index:04}/{name}"),
        display_name: name,
        bytes,
        inline: false,
    }
}

fn read_attachment(path: &Path, remaining: u64) -> io::Result<Vec<u8>> {
    use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};

    let path = std::path::absolute(path)?;
    // A symlink in any component is refused by the kernel, including one
    // swapped in during lookup. NONBLOCK keeps a FIFO from hanging the CLI.
    let fd = openat2(
        CWD,
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS,
    )
    .map_err(|err| {
        if err == rustix::io::Errno::LOOP {
            io::Error::other("symlinks are not accepted in attachment paths")
        } else {
            err.into()
        }
    })?;
    let file = File::from(fd);
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other("expected a regular file"));
    }
    let over_limit = || io::Error::other("attachments exceed the 25 MB sending limit");
    if meta.len() > remaining {
        return Err(over_limit());
    }
    let mut bytes = Vec::new();
    file.take(remaining + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > remaining {
        return Err(over_limit());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_cli(args: &[&str]) -> Result<ComposeLaunch, String> {
        cli(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn rfc_6068_examples_keep_escapes_and_literal_plus() {
        for (uri, expected) in [
            ("mailto:chris@example.com", "chris@example.com"),
            (
                "mailto:gorby%25kremvax@example.com",
                "gorby%kremvax@example.com",
            ),
            (
                "mailto:unlikely%3Faddress@example.com",
                "unlikely?address@example.com",
            ),
            (
                "mailto:%22not%40me%22@example.org",
                "\"not@me\"@example.org",
            ),
            (
                "mailto:Mike%26family@example.org",
                "Mike&family@example.org",
            ),
            (
                "mailto:%22oh%5C%5Cno%22@example.org",
                r#""oh\\no"@example.org"#,
            ),
            ("mailto:bill+ietf@example.org", "bill+ietf@example.org"),
            (
                "mailto:user@%E7%B4%8D%E8%B1%86.example.org?subject=Test&body=NATTO",
                "user@xn--99zt52a.example.org",
            ),
        ] {
            assert_eq!(mailto(uri).unwrap().input.to, [expected]);
        }
        let example = mailto("mailto:user@example.org?subject=caf%C3%A9&body=caf%C3%A9").unwrap();
        assert_eq!(example.input.subject, "café");
        assert_eq!(example.input.body, "café");
    }

    #[test]
    fn mailto_combines_recipient_lists_and_drops_other_headers() {
        let draft = mailto(concat!(
            "MAILTO:a@example.org,b@example.org?",
            "to=c@example.org&%54o=d@example.org%2Ce@example.org",
            "&Cc=f@example.org&BCC=g@example.org",
            "&Subject=One+two%20%26%20three%3Dfour",
            "&From=other@example.org&sender=other@example.org",
            "&attach=/some/file&In-Reply-To=external@example.org"
        ))
        .unwrap();
        let (fields, body) = draft.input.into_fields_and_body("me@example.org".into());
        assert_eq!(
            fields.to,
            [
                "a@example.org",
                "b@example.org",
                "c@example.org",
                "d@example.org",
                "e@example.org"
            ]
        );
        assert_eq!(fields.cc, ["f@example.org"]);
        assert_eq!(fields.bcc, ["g@example.org"]);
        assert_eq!(fields.from, "me@example.org");
        assert_eq!(fields.subject, "One+two & three=four");
        assert_eq!(fields.in_reply_to, None);
        assert!(body.is_empty());
        assert!(draft.files.is_empty());
        assert_eq!(
            mailto("mailto:?to=team@example.org").unwrap().input.to,
            ["team@example.org"]
        );
    }

    #[test]
    fn mailto_body_cannot_create_markdown_images_links_or_styling() {
        use fell_core::compose::{BlockBody, Inline, markdown_to_doc};

        let plain = "# ![photo](photo.png) and [link](https://example.org) **bold**";
        let uri = format!(
            "mailto:?body={}",
            plain.replace('#', "%23").replace(' ', "%20")
        );
        let body = mailto(&uri).unwrap().input.body;
        let doc = markdown_to_doc(&body);
        assert!(matches!(
            &doc.blocks[0].body,
            BlockBody::Paragraph { inlines } if *inlines == [Inline::Text(plain.into())]
        ));
    }

    #[test]
    fn mailto_rejects_injection_and_malformed_fields() {
        for uri in [
            "mailto:one@example.org%0D%0ABcc:extra@example.org",
            "mailto:one@example.org?to=one@example.org%0dBcc:extra@example.org",
            "mailto:one@example.org?cc=one@example.org%0aBcc:extra@example.org",
            "mailto:one@example.org?bcc=one@example.org\nBcc:extra@example.org",
            "mailto:one@example.org?subject=Hi%0D%0ABcc:extra@example.org",
            "mailto:one@example.org?subject=Hi\rBcc:extra@example.org",
            "mailto:one@example.org?body=Line%0D%0ALine",
            "mailto:one@example.org?From=bad%0a",
            "mailto:one@example.org?u%0Dnknown=Hi",
            "mailto:one@example.org?subject=%FF",
            "mailto:one@example.org?body=%2",
            "mailto:one@example.org?subject=%GG",
            "mailto:one@example.org?subject=a&Subject=b",
            "mailto:one@example.org?subject",
            "mailto:one@example.org?cc=two@example.org?body=Hello",
            "mailto:one@example.org#fragment",
            "mailto:one@example.org%2Ctwo@example.org",
        ] {
            assert!(mailto(uri).is_err(), "{uri}");
        }
    }

    #[test]
    fn compose_cli_reads_repeatable_lists_and_multiline_markdown() {
        let launch = parse_cli(&[
            "--to",
            "one@example.org,two@example.org",
            "--to=three@example.org",
            "--cc",
            "\"Example, Person\" <four@example.org>",
            "--bcc=five@example.org",
            "--subject",
            "A subject",
            "--body",
            "**Bold**\r\nNext line",
        ])
        .unwrap();
        assert_eq!(
            launch.input.to,
            ["one@example.org", "two@example.org", "three@example.org"]
        );
        assert_eq!(launch.input.cc, ["\"Example, Person\" <four@example.org>"]);
        assert_eq!(launch.input.bcc, ["five@example.org"]);
        assert_eq!(launch.input.subject, "A subject");
        assert_eq!(launch.input.body, "**Bold**\nNext line");
        for args in [
            &["--bad"][..],
            &["--subject"],
            &["--subject", "--body", "words"],
            &["--to", "not an address"],
            &["--subject", "Line\r\nBcc: extra@example.org"],
        ] {
            assert!(parse_cli(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn compose_cli_copies_readable_files_and_refuses_symlinks_or_over_limit_files() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("report.txt");
        std::fs::write(&file, b"contents").unwrap();
        let launch = cli(&[
            "--attach".into(),
            file.clone().into(),
            "--attach".into(),
            file.into(),
        ])
        .unwrap();
        assert_eq!(launch.files[0].display_name, "report.txt");
        assert_eq!(launch.files[0].bytes, b"contents");
        assert_ne!(launch.files[0].path, launch.files[1].path);

        let file_link = temp.path().join("alias.txt");
        let dir_link = temp.path().join("dir");
        symlink("report.txt", &file_link).unwrap();
        symlink(".", &dir_link).unwrap();
        for link in [file_link, dir_link.join("report.txt")] {
            let err = cli(&["--attach".into(), link.into()]).unwrap_err();
            assert!(err.contains("symlinks"), "{err}");
        }
        assert!(
            cli(&["--attach".into(), temp.path().into()])
                .unwrap_err()
                .contains("regular file")
        );

        let large = temp.path().join("large");
        File::create(&large)
            .unwrap()
            .set_len(MAX_ATTACHMENT_BYTES)
            .unwrap();
        let err = cli(&[
            "--attach".into(),
            temp.path().join("report.txt").into(),
            "--attach".into(),
            large.into(),
        ])
        .unwrap_err();
        assert!(err.contains("25 MB"), "{err}");
    }
}
