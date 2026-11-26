//! WebKit's proxy never resolves a name or opens an upstream socket.
//! The browser's request filter is the first gate; this catches requests
//! made outside the document resource loader, including speculative traffic.

use std::io::Write;
use std::net::TcpListener;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

static PORT: OnceLock<Option<u16>> = OnceLock::new();
static REQUESTS: AtomicUsize = AtomicUsize::new(0);

pub(super) fn port() -> Option<u16> {
    *PORT.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        std::thread::Builder::new()
            .name("fell-web-deny".into())
            .spawn(move || {
                for mut client in listener.incoming().flatten() {
                    REQUESTS.fetch_add(1, Ordering::Relaxed);
                    let _ = client.set_write_timeout(Some(Duration::from_millis(100)));
                    let _ = client.write_all(
                        b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
            })
            .ok()?;
        Some(port)
    })
}

#[cfg(test)]
pub(super) fn requests() -> usize {
    REQUESTS.load(Ordering::Relaxed)
}
