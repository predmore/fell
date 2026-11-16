//! WebKitGTK via wry, painted into the conversation.
//!
//! wry 0.57 can child-embed only into an Xlib window, and GPUI on Wayland
//! does not provide one. The newsletter is laid out in a GTK offscreen
//! window, which Wayland does not map, then snapshotted with JavaScript
//! still disabled and drawn in the message body. There is no title bar and
//! no second copy of the subject. The library stays dynamically linked.

use std::cell::Cell;
use std::cell::RefCell;
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Duration;

use gtk::prelude::*;
use wry::PageLoadEvent;
use wry::WebViewBuilderExtUnix;
use wry::WebViewExtUnix;

#[derive(Clone)]
pub struct WebRequest {
    pub html: String,
    pub token: u64,
    pub background: (u8, u8, u8),
    pub allow_images: bool,
}

enum Cmd {
    Show(WebRequest),
    Hide,
}

struct Host {
    window: gtk::OffscreenWindow,
    webview: wry::WebView,
    token: Cell<u64>,
    /// Bumped on every load and every finished event, so only the latest
    /// snapshot for the current token is stored.
    capture: Cell<u64>,
    pending: RefCell<Option<gtk::glib::SourceId>>,
    fail_safe: RefCell<Option<gtk::glib::SourceId>>,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

static FRAME: Mutex<Option<(u64, Vec<u8>)>> = Mutex::new(None);

pub fn show(request: WebRequest) {
    if let Some(tx) = sender() {
        let _ = tx.send(Cmd::Show(request));
    }
}

pub fn hide() {
    if let Some(tx) = sender() {
        let _ = tx.send(Cmd::Hide);
    }
}

/// Latest full-document PNG for `token`, if WebKit has finished painting it.
pub fn frame_for(token: u64) -> Option<Vec<u8>> {
    FRAME.lock().ok().and_then(|slot| {
        slot.as_ref()
            .filter(|(got, _)| *got == token)
            .map(|(_, png)| png.clone())
    })
}

fn sender() -> Option<&'static gtk::glib::Sender<Cmd>> {
    SENDER.get()
}

static SENDER: OnceLock<gtk::glib::Sender<Cmd>> = OnceLock::new();

fn boot() {
    SENDER.get_or_init(|| {
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("fell-webview".into())
            .spawn(move || gtk_thread(ready_tx))
            .expect("spawn webview thread");
        ready_rx.recv().expect("webkit thread")
    });
}

fn gtk_thread(ready: mpsc::Sender<gtk::glib::Sender<Cmd>>) {
    if let Err(err) = gtk::init() {
        eprintln!("fell: gtk init failed: {err}");
        return;
    }
    #[allow(deprecated)]
    let (tx, rx) = gtk::glib::MainContext::channel(gtk::glib::Priority::DEFAULT);
    if ready.send(tx).is_err() {
        return;
    }
    rx.attach(None, |cmd| {
        match cmd {
            Cmd::Show(request) => present(request),
            Cmd::Hide => HOST.with(|slot| {
                if let Some(host) = slot.borrow().as_ref() {
                    host.window.hide();
                }
            }),
        }
        gtk::glib::ControlFlow::Continue
    });
    gtk::main();
}

fn present(request: WebRequest) {
    let created = HOST.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            match open_host(&request) {
                Ok(host) => *slot = Some(host),
                Err(err) => {
                    eprintln!("fell: web view failed: {err}");
                    return false;
                }
            }
        }
        let Some(host) = slot.as_mut() else {
            return false;
        };
        cancel_source(&host.pending);
        cancel_source(&host.fail_safe);
        host.token.set(request.token);
        host.capture.set(host.capture.get().wrapping_add(1));
        true
    });
    if !created {
        return;
    }
    let (window, widget) = HOST.with(|slot| {
        let slot = slot.borrow();
        let host = slot.as_ref().expect("webview host");
        (host.window.clone(), host.webview.webview())
    });
    window.show_all();
    webkit2gtk::WebViewExt::load_html(&widget, &request.html, None);
    tighten_widget(&widget, request.allow_images);
    arm_fail_safe(request.token);
}

fn open_host(request: &WebRequest) -> wry::Result<Host> {
    let window = gtk::OffscreenWindow::new();
    window.set_focus_on_map(false);
    window.set_accept_focus(false);
    window.set_can_focus(false);
    window.set_default_size(880, 1200);
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    window.add(&vbox);
    window.show_all();
    let (red, green, blue) = request.background;
    let webview = wry::WebViewBuilder::new()
        .with_html(&request.html)
        .with_javascript_disabled()
        .with_autoplay(false)
        .with_clipboard(false)
        .with_background_color((red, green, blue, 255))
        .with_navigation_handler(|uri| !remote_navigation(&uri))
        .with_new_window_req_handler(|_url, _features| wry::NewWindowResponse::Deny)
        .with_on_page_load_handler(|event, _url| {
            if matches!(event, PageLoadEvent::Finished) {
                capture_current();
            }
        })
        .build_gtk(&vbox)?;
    tighten(&webview, request.allow_images);
    Ok(Host {
        window,
        webview,
        token: Cell::new(request.token),
        capture: Cell::new(0),
        pending: RefCell::new(None),
        fail_safe: RefCell::new(None),
    })
}

fn capture_current() {
    let Some((widget, token, capture)) = HOST.with(|slot| {
        let slot = slot.borrow();
        let host = slot.as_ref()?;
        cancel_source(&host.pending);
        let capture = host.capture.get().wrapping_add(1);
        host.capture.set(capture);
        Some((host.webview.webview(), host.token.get(), capture))
    }) else {
        return;
    };
    let id = gtk::glib::timeout_add_local(Duration::from_millis(120), move || {
        HOST.with(|slot| {
            if let Some(host) = slot.borrow().as_ref() {
                let _ = host.pending.borrow_mut().take();
                if host.token.get() == token && host.capture.get() == capture {
                    snapshot(widget.clone(), token, capture);
                }
            }
        });
        gtk::glib::ControlFlow::Break
    });
    HOST.with(|slot| {
        if let Some(host) = slot.borrow().as_ref() {
            if host.token.get() == token && host.capture.get() == capture {
                *host.pending.borrow_mut() = Some(id);
            } else {
                id.remove();
            }
        } else {
            id.remove();
        }
    });
}

fn snapshot(widget: webkit2gtk::WebView, token: u64, capture: u64) {
    use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt};
    widget.snapshot(
        SnapshotRegion::FullDocument,
        SnapshotOptions::NONE,
        None::<&gtk::gio::Cancellable>,
        move |result| {
            let current = HOST.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .map(|host| (host.token.get(), host.capture.get()))
            });
            if current != Some((token, capture)) {
                return;
            }
            if let Ok(surface) = result
                && let Some(png) = surface_png(surface)
                && let Ok(mut frame) = FRAME.lock()
            {
                *frame = Some((token, png));
            }
            HOST.with(|slot| {
                if let Some(host) = slot.borrow().as_ref()
                    && host.token.get() == token
                    && host.capture.get() == capture
                {
                    host.window.hide();
                }
            });
        },
    );
}

fn arm_fail_safe(token: u64) {
    let id = gtk::glib::timeout_add_local(Duration::from_secs(8), move || {
        HOST.with(|slot| {
            if let Some(host) = slot.borrow().as_ref() {
                let _ = host.fail_safe.borrow_mut().take();
                if host.token.get() == token {
                    host.window.hide();
                }
            }
        });
        gtk::glib::ControlFlow::Break
    });
    HOST.with(|slot| {
        if let Some(host) = slot.borrow().as_ref() {
            if host.token.get() == token {
                *host.fail_safe.borrow_mut() = Some(id);
            } else {
                id.remove();
            }
        } else {
            id.remove();
        }
    });
}

fn cancel_source(slot: &RefCell<Option<gtk::glib::SourceId>>) {
    if let Some(id) = slot.borrow_mut().take() {
        id.remove();
    }
}

fn tighten_widget(widget: &webkit2gtk::WebView, allow_images: bool) {
    use webkit2gtk::{SettingsExt, WebViewExt};
    if let Some(settings) = WebViewExt::settings(widget) {
        settings.set_enable_javascript(false);
        settings.set_auto_load_images(allow_images);
        settings.set_enable_developer_extras(false);
        settings.set_javascript_can_open_windows_automatically(false);
        settings.set_enable_webaudio(false);
        settings.set_enable_webgl(false);
    }
}

fn surface_png(surface: gtk::cairo::Surface) -> Option<Vec<u8>> {
    let mut image = gtk::cairo::ImageSurface::try_from(surface).ok()?;
    image.flush();
    let width = image.width();
    let height = image.height();
    if width <= 0 || height <= 0 {
        return None;
    }
    let stride = image.stride() as usize;
    let data = image.data().ok()?;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height as usize {
        let start = y * stride;
        let row = data.get(start..start + width as usize * 4)?;
        for px in row.as_chunks::<4>().0 {
            let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
            if a == 0 || a == 255 {
                rgba.extend_from_slice(&[r, g, b, a]);
            } else {
                rgba.extend_from_slice(&[
                    ((u16::from(r) * 255) / u16::from(a)) as u8,
                    ((u16::from(g) * 255) / u16::from(a)) as u8,
                    ((u16::from(b) * 255) / u16::from(a)) as u8,
                    a,
                ]);
            }
        }
    }
    let buffer = image::RgbaImage::from_raw(width as u32, height as u32, rgba)?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(buffer)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

fn tighten(webview: &wry::WebView, allow_images: bool) {
    use webkit2gtk::{SettingsExt, WebViewExt};
    let widget = webview.webview();
    if let Some(settings) = WebViewExt::settings(&widget) {
        settings.set_enable_javascript(false);
        settings.set_auto_load_images(allow_images);
        settings.set_enable_developer_extras(false);
        settings.set_javascript_can_open_windows_automatically(false);
        settings.set_enable_webaudio(false);
        settings.set_enable_webgl(false);
    }
}

fn remote_navigation(uri: &str) -> bool {
    let uri = uri.to_ascii_lowercase();
    uri.starts_with("http://") || uri.starts_with("https://") || uri.starts_with("file:")
}

pub fn ensure_started() {
    boot();
}
