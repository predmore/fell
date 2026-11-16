//! WebKitGTK via wry, painted into the conversation.
//!
//! wry 0.57 can child-embed only into an Xlib window, and GPUI on Wayland
//! does not provide one. The newsletter is laid out in an undecorated GTK
//! window kept off-screen, snapshotted with JavaScript still disabled, and
//! drawn in the message body. There is no title bar, no traffic-light
//! chrome, and no second copy of the subject. The library stays dynamically
//! linked.

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
    window: gtk::Window,
    webview: wry::WebView,
    token: Cell<u64>,
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
    HOST.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            match open_host(&request) {
                Ok(host) => *slot = Some(host),
                Err(err) => {
                    eprintln!("fell: web view failed: {err}");
                    return;
                }
            }
        }
        let Some(host) = slot.as_mut() else {
            return;
        };
        host.token.set(request.token);
        if let Err(err) = host.webview.load_html(&request.html) {
            eprintln!("fell: web view load failed: {err}");
        }
        tighten(&host.webview, request.allow_images);
        host.window.show_all();
    });
}

fn open_host(request: &WebRequest) -> wry::Result<Host> {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_decorated(false);
    window.set_resizable(false);
    window.set_skip_taskbar_hint(true);
    window.set_skip_pager_hint(true);
    window.set_default_size(880, 480);
    #[allow(deprecated)]
    window.move_(-4000, -4000);
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
    })
}

fn capture_current() {
    let Some((widget, token)) = HOST.with(|slot| {
        slot.borrow().as_ref().map(|host| {
            let widget = host.webview.webview();
            (widget, host.token.get())
        })
    }) else {
        return;
    };
    // Let WebKit finish layout before the full-document snapshot.
    gtk::glib::timeout_add_local(Duration::from_millis(120), move || {
        snapshot(widget.clone(), token);
        gtk::glib::ControlFlow::Break
    });
}

fn snapshot(widget: webkit2gtk::WebView, token: u64) {
    use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt};
    widget.snapshot(
        SnapshotRegion::FullDocument,
        SnapshotOptions::NONE,
        None::<&gtk::gio::Cancellable>,
        move |result| {
            if let Ok(surface) = result
                && let Some(png) = surface_png(surface)
                && let Ok(mut slot) = FRAME.lock()
            {
                *slot = Some((token, png));
            }
            HOST.with(|slot| {
                if let Some(host) = slot.borrow().as_ref() {
                    host.window.hide();
                }
            });
        },
    );
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
