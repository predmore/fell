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
const MIN_DATA_URI_BYTES: usize = "data:image/png;base64,AAAA".len();
const FETCH_TIME: Duration = Duration::from_secs(3);
const PAGE_TIME: Duration = Duration::from_secs(12);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Unreachable(String),
    Address,
    Response,
    NotFound,
    Format,
    Limit,
    PageLimit,
}

impl Failure {
    fn label(&self, compact: bool) -> String {
        match self {
            Self::Unreachable(_) if compact => "Couldn’t reach server".into(),
            Self::Unreachable(host) => format!("Couldn’t reach {host}"),
            Self::NotFound => "Image not found".into(),
            Self::Response if compact => "Server error".into(),
            Self::Format if compact => "Unsupported format".into(),
            Self::Address => "Image address blocked".into(),
            Self::Response => "Image server didn’t return an image".into(),
            Self::Format => "Unsupported or damaged image".into(),
            Self::Limit => "Size limit".into(),
            Self::PageLimit => "Download limit reached".into(),
        }
    }

    fn retryable(&self) -> bool {
        !matches!(self, Self::Limit | Self::PageLimit)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct UnavailableImages {
    first: Failure,
    count: usize,
    retryable: bool,
}

impl UnavailableImages {
    pub(crate) fn label(&self, compact: bool) -> String {
        let noun = if self.count == 1 { "image" } else { "images" };
        match (&self.first, self.count) {
            (Failure::Limit, 1) => "1 image too large".into(),
            (Failure::PageLimit | Failure::NotFound, 1) => self.first.label(compact),
            _ => format!(
                "{} {noun} unavailable · {}",
                self.count,
                self.first.label(compact)
            ),
        }
    }

    pub(crate) fn retryable(&self) -> bool {
        self.retryable
    }
}

pub(crate) fn inline_page(
    mail: &fell_render::WebMail,
    scheme: &str,
    allowed: impl Fn() -> bool,
) -> (String, Result<(), UnavailableImages>) {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    let deadline = Instant::now() + PAGE_TIME;
    inline_page_with(
        mail,
        scheme,
        PAGE_BYTES,
        deadline,
        &allowed,
        |url, limit, bytes_read| {
            fetch(
                AGENT.get_or_init(agent),
                url,
                limit,
                deadline,
                &allowed,
                bytes_read,
            )
        },
    )
}

fn inline_page_with(
    mail: &fell_render::WebMail,
    scheme: &str,
    mut remaining: usize,
    deadline: Instant,
    allowed: impl Fn() -> bool,
    mut fetch: impl FnMut(&str, usize, &mut usize) -> Result<String, Failure>,
) -> (String, Result<(), UnavailableImages>) {
    let mut fetched: HashMap<String, Result<String, Failure>> = HashMap::new();
    let mut unavailable = None;
    let mut limited = false;
    let mut failed = |error: Failure| {
        if error == Failure::PageLimit && std::mem::replace(&mut limited, true) {
            return None;
        }
        let retryable = error.retryable();
        let failure = unavailable.get_or_insert(UnavailableImages {
            first: error,
            count: 0,
            retryable: false,
        });
        failure.count += 1;
        failure.retryable |= retryable;
        None
    };
    let html = mail.page(scheme, |url| {
        if !allowed() {
            return None;
        }
        if remaining < MIN_DATA_URI_BYTES {
            return failed(Failure::PageLimit);
        }
        let image = match fetched.entry(url.to_owned()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let mut bytes_read = 0;
                let result = if Instant::now() >= deadline {
                    Err(unreachable(Url::parse(url).ok().as_ref()))
                } else {
                    fetch(url, remaining, &mut bytes_read)
                };
                let image = entry.insert(result);
                if let Err(error) = image {
                    // Failed bodies still spent network bytes. A successful
                    // one is charged its larger encoded length below.
                    remaining = remaining.saturating_sub(bytes_read);
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
            Ok(_) => failed(Failure::PageLimit),
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
    ureq::Agent::with_parts(
        agent_config(tls),
        DefaultConnector::default(),
        PublicResolver,
    )
}

fn agent_config(tls: ureq::tls::TlsConfig) -> ureq::config::Config {
    ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .max_idle_connections(0)
        .timeout_global(Some(FETCH_TIME))
        .timeout_resolve(Some(FETCH_TIME))
        .tls_config(tls)
        .build()
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

fn unreachable(url: Option<&Url>) -> Failure {
    Failure::Unreachable(url.and_then(Url::host_str).unwrap_or_default().to_owned())
}

fn fetch(
    agent: &ureq::Agent,
    source: &str,
    encoded_limit: usize,
    deadline: Instant,
    allowed: &impl Fn() -> bool,
    bytes_read: &mut usize,
) -> Result<String, Failure> {
    let mut url = Url::parse(source).map_err(|_| Failure::Address)?;
    let deadline = deadline.min(Instant::now() + FETCH_TIME);
    for _ in 0..=3 {
        url.set_fragment(None);
        let time = deadline.saturating_duration_since(Instant::now());
        if time.is_zero() {
            return Err(unreachable(Some(&url)));
        }
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
                ureq::Error::StatusCode(404) => Failure::NotFound,
                ureq::Error::StatusCode(_) => Failure::Response,
                _ => unreachable(Some(&url)),
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
        let mut data = format!("data:{mime};base64,");
        // Base64 takes four bytes per three input bytes, including padding.
        let limit = (encoded_limit.saturating_sub(data.len()) / 4 * 3).min(IMAGE_BYTES);
        if limit == 0 {
            return Err(Failure::PageLimit);
        }
        let mut bytes = Vec::new();
        let result = response
            .into_body()
            .into_reader()
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes);
        *bytes_read = bytes.len();
        result.map_err(|_| unreachable(Some(&url)))?;
        if !allowed() || bytes.len() > limit {
            return Err(Failure::Limit);
        }
        if !super::native::raster_bytes_allowed(&bytes, mime) {
            return Err(Failure::Format);
        }
        base64::engine::general_purpose::STANDARD.encode_string(bytes, &mut data);
        return Ok(data);
    }
    Err(Failure::Response)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{BufRead, BufReader, Write};
    use std::net::{Ipv4Addr, TcpListener, TcpStream};

    const HOST: &str = "images.example.test";

    #[derive(Debug)]
    struct LoopbackResolver;

    impl Resolver for LoopbackResolver {
        fn resolve(
            &self,
            uri: &ureq::http::Uri,
            _: &ureq::config::Config,
            _: NextTimeout,
        ) -> Result<ResolvedSocketAddrs, ureq::Error> {
            // Only the synthetic public hostname is routed to the fixture.
            // fetch still validates every URL before issuing a request.
            assert_eq!(uri.host(), Some(HOST), "{uri}");
            let mut addresses = self.empty();
            addresses.push((Ipv4Addr::LOCALHOST, uri.port_u16().unwrap()).into());
            Ok(addresses)
        }
    }

    fn accept(listener: &TcpListener) -> TcpStream {
        let deadline = Instant::now() + FETCH_TIME;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_read_timeout(Some(FETCH_TIME)).unwrap();
                    stream.set_write_timeout(Some(FETCH_TIME)).unwrap();
                    return stream;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("waiting for fixture request: {error}"),
            }
        }
    }

    fn respond(mut stream: impl Read + Write, path: &str, headers: &str, body: &[u8]) {
        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, format!("GET {path} HTTP/1.1\r\n"));
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
        }
        write!(
            stream,
            "HTTP/1.1 {headers}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len(),
        )
        .unwrap();
        if let Err(error) = stream.write_all(body).and_then(|()| stream.flush()) {
            assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                ),
                "writing fixture response: {error}"
            );
        }
    }

    #[test]
    fn fetch_follows_http_redirects_and_checks_each_target() {
        use rustls::pki_types::PrivateKeyDer;
        use rustls::{ServerConfig, ServerConnection, StreamOwned};
        use ureq::tls::{Certificate, RootCerts, TlsConfig};

        let http = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let https = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        http.set_nonblocking(true).unwrap();
        https.set_nonblocking(true).unwrap();
        let port = http.local_addr().unwrap().port();
        let https_port = https.local_addr().unwrap().port();
        let fixture = rcgen::generate_simple_self_signed(vec![HOST.into()]).unwrap();
        let cert = fixture.cert.der();
        let key = PrivateKeyDer::Pkcs8(fixture.key_pair.serialize_der().into());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key)
            .unwrap();
        let tls = TlsConfig::builder()
            .root_certs(RootCerts::new_with_certs(&[Certificate::from_der(
                cert.as_ref(),
            )
            .to_owned()]))
            .unversioned_rustls_crypto_provider(provider)
            .build();
        let client = ureq::Agent::with_parts(
            agent_config(tls),
            DefaultConnector::default(),
            LoopbackResolver,
        );
        let encoded = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7"; // 1 × 1 GIF
        let gif = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let data = format!("data:image/gif;base64,{encoded}");
        let http_url = |path| format!("http://{HOST}:{port}{path}");

        std::thread::scope(|scope| {
            scope.spawn(|| {
                for (path, header) in [
                    (
                        "/start",
                        "301 Moved Permanently\r\nLocation: /next".to_owned(),
                    ),
                    ("/next", "302 Found\r\nLocation: /image".to_owned()),
                ] {
                    respond(accept(&http), path, &header, b"");
                }
                for _ in 0..2 {
                    respond(
                        accept(&http),
                        "/image",
                        "200 OK\r\nContent-Type: image/gif",
                        &gif,
                    );
                }
                for (path, target) in [
                    ("/loopback", format!("http://127.0.0.1:{port}/image")),
                    ("/private", "http://10.0.0.1/image".into()),
                    ("/scheme", format!("ftp://{HOST}:{port}/image")),
                ] {
                    respond(
                        accept(&http),
                        path,
                        &format!("302 Found\r\nLocation: {target}"),
                        b"",
                    );
                }
            });
            scope.spawn(|| {
                let conn = ServerConnection::new(Arc::new(server)).unwrap();
                let tls = StreamOwned::new(conn, accept(&https));
                respond(
                    tls,
                    "/downgrade",
                    &format!("302 Found\r\nLocation: {}", http_url("/image")),
                    b"",
                );
            });
            let deadline = Instant::now() + PAGE_TIME;
            assert_eq!(
                fetch(
                    &client,
                    &http_url("/start"),
                    data.len(),
                    deadline,
                    &|| true,
                    &mut 0
                ),
                Ok(data.clone())
            );
            assert_eq!(
                fetch(
                    &client,
                    &http_url("/image"),
                    data.len() - 1,
                    deadline,
                    &|| true,
                    &mut 0
                ),
                Err(Failure::Limit)
            );
            for url in [
                http_url("/loopback"),
                http_url("/private"),
                http_url("/scheme"),
                format!("https://{HOST}:{https_port}/downgrade"),
            ] {
                assert_eq!(
                    fetch(&client, &url, PAGE_BYTES, deadline, &|| true, &mut 0),
                    Err(Failure::Address),
                    "{url}"
                );
            }
        });
    }

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
                Instant::now() + PAGE_TIME,
                || true,
                |url, _, _| {
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
                    count: 2,
                    retryable: true
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
            Instant::now() + PAGE_TIME,
            || true,
            |url, _, _| {
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
                count: 2,
                retryable: true
            })
        ));
    }

    #[test]
    fn images_that_miss_the_budget_do_not_stop_smaller_downloads() {
        let mail = fell_render::WebMail::prepare(concat!(
            r#"<img src="https://example.test/big"><img src="https://example.test/larger">"#,
            r#"<img src="https://example.test/small">"#,
        ));
        let small = "data:image/png;base64,AAAA";
        let mut requested = Vec::new();
        let (page, result) = inline_page_with(
            &mail,
            "light",
            PAGE_BYTES,
            Instant::now() + PAGE_TIME,
            || true,
            |url, limit, bytes_read| {
                requested.push(url.rsplit('/').next().unwrap().to_owned());
                match requested.last().unwrap().as_str() {
                    "big" | "larger" => {
                        *bytes_read = IMAGE_BYTES + 1;
                        Err(Failure::Limit)
                    }
                    _ => {
                        assert_eq!(limit, PAGE_BYTES - 2 * (IMAGE_BYTES + 1));
                        Ok(small.into())
                    }
                }
            },
        );
        assert_eq!(requested, ["big", "larger", "small"]);
        assert_eq!(page.matches(small).count(), 1);
        assert!(matches!(
            result,
            Err(UnavailableImages {
                first: Failure::Limit,
                count: 2,
                retryable: false
            })
        ));
    }

    #[test]
    fn many_oversized_responses_cannot_spend_the_page_budget_repeatedly() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut html = String::new();
        for index in 0..48 {
            html.push_str(&format!(r#"<img src="http://{HOST}:{port}/{index}">"#));
        }
        let mail = fell_render::WebMail::prepare(&html);
        let client = ureq::Agent::with_parts(
            agent_config(ureq::tls::TlsConfig::default()),
            DefaultConnector::default(),
            LoopbackResolver,
        );
        let mut requested = 0;
        let mut read = 0;
        let body = vec![0; IMAGE_BYTES + 1];
        let (_, result) = inline_page_with(
            &mail,
            "light",
            PAGE_BYTES,
            Instant::now() + PAGE_TIME,
            || true,
            |url, limit, bytes_read| {
                let path = format!("/{requested}");
                let result = std::thread::scope(|scope| {
                    scope.spawn(|| {
                        respond(
                            accept(&listener),
                            &path,
                            "200 OK\r\nContent-Type: image/png",
                            &body,
                        );
                    });
                    fetch(
                        &client,
                        url,
                        limit,
                        Instant::now() + FETCH_TIME,
                        &|| true,
                        bytes_read,
                    )
                });
                assert_eq!(result, Err(Failure::Limit));
                requested += 1;
                read += *bytes_read;
                result
            },
        );

        assert!(result.is_err_and(|failure| !failure.retryable()));
        assert!((3..48).contains(&requested), "{requested} requests");
        assert!(
            (PAGE_BYTES - MIN_DATA_URI_BYTES..=PAGE_BYTES + IMAGE_BYTES).contains(&read),
            "{read} bytes"
        );
    }
}
