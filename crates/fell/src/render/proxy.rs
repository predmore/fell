//! Filtering proxy in front of WebKit.
//!
//! Images, stylesheets, and fonts are fetched by the web process. Rewriting a
//! `URIRequest` in `resource-load-started` does not change that fetch, and
//! `sent-request` runs after the bytes are already on the wire. Every request,
//! including the one that follows a redirect, comes through here instead.
//! The socket is opened to an address that was already accepted, and the peer
//! address is checked again so a name cannot rebind onto a blocked network.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::OnceLock;
use std::time::Duration;

static PORT: OnceLock<u16> = OnceLock::new();

pub fn port() -> u16 {
    *PORT.get_or_init(|| listen().expect("fell image proxy"))
}

fn listen() -> std::io::Result<u16> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
    let port = listener.local_addr()?.port();
    std::thread::Builder::new()
        .name("fell-proxy".into())
        .spawn(move || {
            for conn in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = serve(conn);
                });
            }
        })
        .map_err(std::io::Error::other)?;
    Ok(port)
}

fn serve(mut client: TcpStream) -> std::io::Result<()> {
    let _ = client.set_read_timeout(Some(Duration::from_secs(5)));
    let header = read_headers(&mut client)?;
    let text = String::from_utf8_lossy(&header);
    let Some(line) = text.lines().next() else {
        return Ok(());
    };
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    if method.eq_ignore_ascii_case("CONNECT") {
        let Some((host, port)) = split_host_port(target, 443) else {
            deny(&mut client)?;
            return Ok(());
        };
        match dial(&host, port) {
            Ok(upstream) => tunnel(client, upstream),
            Err(()) => deny(&mut client),
        }
    } else {
        let Some((host, port, path)) = split_absolute(target) else {
            deny(&mut client)?;
            return Ok(());
        };
        let Ok(mut upstream) = dial(&host, port) else {
            deny(&mut client)?;
            return Ok(());
        };
        let origin =
            format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        upstream.write_all(origin.as_bytes())?;
        let _ = std::io::copy(&mut upstream, &mut client);
        Ok(())
    }
}

fn deny(client: &mut TcpStream) -> std::io::Result<()> {
    client.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

/// Connect only to an address this process has already accepted.
fn dial(host: &str, port: u16) -> Result<TcpStream, ()> {
    let targets = if let Ok(ip) = host.parse::<IpAddr>() {
        if fell_render::address_is_blocked(ip) {
            return Err(());
        }
        vec![SocketAddr::new(ip, port)]
    } else {
        let addresses = (host, port)
            .to_socket_addrs()
            .map(|addrs| addrs.collect::<Vec<_>>())
            .unwrap_or_default();
        if addresses.is_empty()
            || addresses
                .iter()
                .any(|addr| fell_render::address_is_blocked(addr.ip()))
        {
            return Err(());
        }
        addresses
    };
    for addr in targets {
        let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(4)) else {
            continue;
        };
        let peer_blocked = stream
            .peer_addr()
            .ok()
            .is_some_and(|peer| fell_render::address_is_blocked(peer.ip()));
        if peer_blocked {
            continue;
        }
        return Ok(stream);
    }
    Err(())
}

fn tunnel(mut client: TcpStream, mut upstream: TcpStream) -> std::io::Result<()> {
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    let mut outbound = upstream.try_clone()?;
    let mut inbound = client.try_clone()?;
    let copied = std::thread::spawn(move || {
        let _ = std::io::copy(&mut inbound, &mut outbound);
    });
    let _ = std::io::copy(&mut upstream, &mut client);
    let _ = copied.join();
    Ok(())
}

fn read_headers(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while buf.len() < 64 * 1024 {
        if stream.read(&mut byte)? == 0 {
            break;
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    Ok(buf)
}

fn split_host_port(target: &str, default_port: u16) -> Option<(String, u16)> {
    if let Some(host) = target.strip_prefix('[') {
        let (host, rest) = host.split_once(']')?;
        let port = rest
            .strip_prefix(':')
            .and_then(|port| port.parse().ok())
            .unwrap_or(default_port);
        return Some((host.to_string(), port));
    }
    if let Some((host, port)) = target.rsplit_once(':')
        && let Ok(port) = port.parse()
    {
        return Some((host.to_string(), port));
    }
    Some((target.to_string(), default_port))
}

fn split_absolute(target: &str) -> Option<(String, u16, String)> {
    let rest = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("https://"))?;
    let default_port = if target.starts_with("https://") {
        443
    } else {
        80
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let (host, port) = split_host_port(authority, default_port)?;
    Some((host, port, path.to_string()))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn dial_does_not_open_a_loopback_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&hits);
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                counted.fetch_add(1, Ordering::SeqCst);
                let _ = stream.write_all(b"nope");
            }
        });
        assert!(super::dial("127.0.0.1", port).is_err());
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn proxy_refuses_a_loopback_get_and_still_forwards_a_public_address() {
        let blocked = TcpListener::bind("127.0.0.1:0").unwrap();
        let blocked_port = blocked.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&hits);
        std::thread::spawn(move || {
            for conn in blocked.incoming().flatten() {
                counted.fetch_add(1, Ordering::SeqCst);
                let mut conn = conn;
                let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi");
            }
        });
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = proxy.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = proxy.accept() {
                let _ = super::serve(stream);
            }
        });
        let mut client = std::net::TcpStream::connect(("127.0.0.1", proxy_port)).unwrap();
        let request = format!(
            "GET http://127.0.0.1:{blocked_port}/img.png HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
        );
        client.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        let _ = client.read_to_string(&mut response);
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }
}
