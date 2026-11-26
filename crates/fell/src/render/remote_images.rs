//! The only HTTP client that reads remote content for the message renderer.

use std::collections::HashMap;
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

pub(crate) fn inline_page(
    mail: &fell_render::WebMail,
    scheme: &str,
    allowed: impl Fn() -> bool,
) -> (String, usize) {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    let deadline = Instant::now() + PAGE_TIME;
    let mut remaining = PAGE_BYTES;
    let mut fetched: HashMap<String, Option<String>> = HashMap::new();
    let html = mail.page(scheme, |url| {
        if !allowed() || Instant::now() >= deadline || remaining == 0 {
            return None;
        }
        fetched
            .entry(url.to_owned())
            .or_insert_with(|| {
                let data = fetch(
                    AGENT.get_or_init(agent),
                    url,
                    remaining.min(IMAGE_BYTES),
                    &allowed,
                )?;
                Some(data)
            })
            .as_ref()
            .filter(|data| data.len() <= remaining)
            .map(|data| {
                remaining -= data.len();
                data.clone()
            })
    });
    (
        html,
        fetched.values().filter(|value| value.is_some()).count(),
    )
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
) -> Option<String> {
    let mut url = Url::parse(source).ok()?;
    let started = Instant::now();
    for _ in 0..=3 {
        let time = FETCH_TIME.checked_sub(started.elapsed())?;
        if !allowed()
            || !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || fell_render::targets_private_network(url.as_str())
        {
            return None;
        }
        url.set_fragment(None);
        // No cookie jar, credentials, client referrer, or redirect headers.
        let response = agent
            .get(url.as_str())
            .header("Accept", "image/png,image/jpeg,image/gif")
            .config()
            .timeout_global(Some(time))
            .build()
            .call()
            .ok()?;
        if response.status().is_redirection() {
            let next = url
                .join(response.headers().get("location")?.to_str().ok()?)
                .ok()?;
            if url.scheme() == "https" && next.scheme() != "https" {
                return None;
            }
            url = next;
            continue;
        }
        let mime = match response.body().mime_type()? {
            "image/png" => "image/png",
            "image/jpeg" => "image/jpeg",
            "image/gif" => "image/gif",
            _ => return None,
        };
        let mut bytes = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if !allowed() || bytes.len() > limit || !super::native::raster_bytes_allowed(&bytes, mime) {
            return None;
        }
        return Some(format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ));
    }
    None
}
