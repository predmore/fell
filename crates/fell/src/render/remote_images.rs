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

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    Unreachable(String),
    Address,
    Response,
    Format,
    Limit,
}

impl Failure {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Unreachable(host) => format!("Couldn’t reach {host}"),
            Self::Address => "Image address blocked".into(),
            Self::Response => "Image server didn’t return an image".into(),
            Self::Format => "Unsupported or damaged image".into(),
            Self::Limit => "Image download limit reached".into(),
        }
    }
}

pub(crate) fn inline_page(
    mail: &fell_render::WebMail,
    scheme: &str,
    allowed: impl Fn() -> bool,
) -> (String, Result<(), Failure>) {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    let deadline = Instant::now() + PAGE_TIME;
    let mut remaining = PAGE_BYTES;
    let mut fetched: HashMap<String, Result<String, Failure>> = HashMap::new();
    let mut result = Err(Failure::Limit);
    let html = mail.page(scheme, |url| {
        if !allowed() || Instant::now() >= deadline || remaining == 0 {
            return None;
        }
        let image = fetched.entry(url.to_owned()).or_insert_with(|| {
            fetch(
                AGENT.get_or_init(agent),
                url,
                remaining.min(IMAGE_BYTES),
                &allowed,
            )
        });
        match image {
            Ok(data) if data.len() <= remaining => {
                result = Ok(());
                remaining -= data.len();
                Some(data.clone())
            }
            Err(failure) if matches!(result, Err(Failure::Limit)) => {
                result = Err(failure.clone());
                None
            }
            _ => None,
        }
    });
    (html, result)
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
