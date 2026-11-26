//! The only HTTP client that reads remote content for the message renderer.

use std::collections::{HashMap, hash_map::Entry};
use std::io::Read;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine;
use ureq::unversioned::{
    resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver},
    transport::{DefaultConnector, NextTimeout},
};
use url::Url;

const IMAGE_BYTES: usize = 4 * 1024 * 1024;
const PAGE_BYTES: usize = 12 * 1024 * 1024;
const FETCH_TIME: Duration = Duration::from_secs(3);
const PAGE_TIME: Duration = Duration::from_secs(12);

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    Unreachable(String),
    Address,
    Response,
    Format,
    Limit,
}

impl Failure {
    fn label(&self) -> String {
        match self {
            Self::Unreachable(host) => format!("Couldn’t reach {host}"),
            Self::Address => "Image address blocked".into(),
            Self::Response => "Image server didn’t return an image".into(),
            Self::Format => "Unsupported or damaged image".into(),
            Self::Limit => "Image download limit reached".into(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct UnavailableImages {
    first: Failure,
    count: usize,
}

impl UnavailableImages {
    pub(crate) fn label(&self) -> String {
        let noun = if self.count == 1 { "image" } else { "images" };
        format!("{} {noun} unavailable · {}", self.count, self.first.label())
    }
}

pub(crate) fn inline_page(
    mail: &fell_render::WebMail,
    scheme: &str,
    allowed: impl Fn() -> bool,
) -> (String, Result<(), UnavailableImages>) {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    inline_page_with(mail, scheme, PAGE_BYTES, &allowed, |url, limit| {
        fetch(AGENT.get_or_init(agent), url, limit, &allowed)
    })
}

fn inline_page_with(
    mail: &fell_render::WebMail,
    scheme: &str,
    mut remaining: usize,
    allowed: impl Fn() -> bool,
    mut fetch: impl FnMut(&str, usize) -> Result<String, Failure>,
) -> (String, Result<(), UnavailableImages>) {
    let deadline = Instant::now() + PAGE_TIME;
    let mut fetched: HashMap<String, Result<String, Failure>> = HashMap::new();
    let mut unavailable = None;
    let mut limited = false;
    let mut failed = |error| {
        let failure = unavailable.get_or_insert(UnavailableImages {
            first: error,
            count: 0,
        });
        failure.count += 1;
        None
    };
    let html = mail.page(scheme, |url| {
        if limited {
            return None;
        }
        if !allowed() || Instant::now() >= deadline || remaining == 0 {
            limited = true;
            return failed(Failure::Limit);
        }
        let image = match fetched.entry(url.to_owned()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let image = entry.insert(fetch(url, remaining.min(IMAGE_BYTES)));
                if let Err(error) = image {
                    return failed(error.clone());
                }
                image
            }
        };
        match image {
            Ok(data) if data.len() <= remaining => {
                remaining -= data.len();
                Some(data.clone())
            }
            Ok(_) => {
                limited = true;
                failed(Failure::Limit)
            }
            Err(_) => None,
        }
    });
    (html, unavailable.map_or(Ok(()), Err))
}

fn agent() -> ureq::Agent {
    use ureq::tls::{Certificate, RootCerts, TlsConfig};
    let roots = rustls_native_certs::load_native_certs()
        .certs
        .iter()
        .map(|cert| Certificate::from_der(cert.as_ref()).to_owned())
        .collect::<Vec<_>>();
    let tls = TlsConfig::builder()
        .root_certs(RootCerts::new_with_certs(&roots))
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    let config = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .max_idle_connections(0)
        .timeout_global(Some(FETCH_TIME))
        .timeout_resolve(Some(FETCH_TIME))
        .tls_config(tls)
        .build();
    ureq::Agent::with_parts(config, DefaultConnector::default(), PublicResolver)
}

#[derive(Debug)]
struct PublicResolver;

impl Resolver for PublicResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let addresses = DefaultResolver::default().resolve(uri, config, timeout)?;
        // With no proxy the TCP connector uses these exact addresses. A
        // second DNS lookup cannot replace them with a private address.
        if addresses
            .iter()
            .any(|addr| fell_render::address_is_blocked(addr.ip()))
        {
            return Err(ureq::Error::HostNotFound);
        }
        Ok(addresses)
    }
}

fn fetch(
    agent: &ureq::Agent,
    source: &str,
    limit: usize,
    allowed: &impl Fn() -> bool,
) -> Result<String, Failure> {
    let mut url = Url::parse(source).map_err(|_| Failure::Address)?;
    let started = Instant::now();
    for _ in 0..=3 {
        url.set_fragment(None);
        let unreachable = || Failure::Unreachable(url.host_str().unwrap_or_default().to_owned());
        let time = FETCH_TIME
            .checked_sub(started.elapsed())
            .ok_or_else(unreachable)?;
        if !allowed()
            || !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || fell_render::targets_private_network(url.as_str())
        {
            return Err(Failure::Address);
        }
        // No cookie jar, credentials, client referrer, or redirect headers.
        let response = agent
            .get(url.as_str())
            .header("Accept", "image/png,image/jpeg,image/gif")
            .config()
            .timeout_global(Some(time))
            .build()
            .call()
            .map_err(|error| match error {
                ureq::Error::StatusCode(_) => Failure::Response,
                _ => unreachable(),
            })?;
        if response.status().is_redirection() {
            let target = response
                .headers()
                .get("location")
                .and_then(|header| header.to_str().ok())
                .ok_or(Failure::Response)?;
            let next = url.join(target).map_err(|_| Failure::Address)?;
            if url.scheme() == "https" && next.scheme() != "https" {
                return Err(Failure::Address);
            }
            url = next;
            continue;
        }
        let mime = match response.body().mime_type() {
            Some("image/png") => "image/png",
            Some("image/jpeg") => "image/jpeg",
            Some("image/gif") => "image/gif",
            _ => return Err(Failure::Format),
        };
        let mut bytes = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| unreachable())?;
        if !allowed() || bytes.len() > limit {
            return Err(Failure::Limit);
        }
        if !super::native::raster_bytes_allowed(&bytes, mime) {
            return Err(Failure::Format);
        }
        return Ok(format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ));
    }
    Err(Failure::Response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_page_keeps_the_first_image_failure() {
        let urls = [
            "https://example.test/good",
            "https://example.test/missing",
            "https://example.test/damaged",
        ];
        for order in [[0, 1, 2], [1, 0, 2]] {
            let html = order
                .map(|index| format!(r#"<img src="{}">"#, urls[index]))
                .concat();
            let mail = fell_render::WebMail::prepare(&html);
            let (page, result) = inline_page_with(
                &mail,
                "light",
                PAGE_BYTES,
                || true,
                |url, _| {
                    if url == urls[0] {
                        Ok("data:image/png;base64,AAAA".into())
                    } else if url == urls[1] {
                        Err(Failure::Response)
                    } else {
                        Err(Failure::Format)
                    }
                },
            );
            assert!(page.contains("data:image/png;base64,AAAA"));
            assert!(matches!(
                result,
                Err(UnavailableImages {
                    first: Failure::Response,
                    count: 2
                })
            ));
        }
    }

    #[test]
    fn duplicate_failures_count_once_and_exhaustion_is_one_failure() {
        let mail = fell_render::WebMail::prepare(concat!(
            r#"<img src="https://example.test/bad"><img src="https://example.test/bad">"#,
            r#"<img src="https://example.test/good"><img src="https://example.test/skipped">"#,
            r#"<img src="https://example.test/good"><img src="https://example.test/also-skipped">"#,
        ));
        let data = "data:image/png;base64,AAAA";
        let (page, result) = inline_page_with(
            &mail,
            "light",
            data.len(),
            || true,
            |url, _| {
                if url.ends_with("/bad") {
                    Err(Failure::Response)
                } else {
                    assert!(url.ends_with("/good"), "{url}");
                    Ok(data.into())
                }
            },
        );
        assert_eq!(page.matches(data).count(), 1);
        assert!(matches!(
            result,
            Err(UnavailableImages {
                first: Failure::Response,
                count: 2
            })
        ));
    }
}
