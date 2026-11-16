//! WebKitGTK, via wry, in its own window.
//!
//! wry 0.57 can child-embed only into an Xlib window. GPUI on Wayland hands
//! back a Wayland surface, so the newsletter view is a GTK window created
//! with `WebViewBuilderExtUnix::build_gtk`. The library stays dynamically
//! linked. JavaScript is off and remote images stay blocked until allowed.
//!
//! The inbox does not own this window. It calls [`show`] and [`hide`] when
//! the message body toggles.

use std::cell::RefCell;
use std::sync::{OnceLock, mpsc};

use gtk::prelude::*;
use wry::WebViewBuilderExtUnix;
use wry::WebViewExtUnix;

#[derive(Clone)]
pub struct WebRequest {
    pub html: String,
    pub title: String,
    pub background: (u8, u8, u8),
    pub foreground: (u8, u8, u8),
    pub allow_images: bool,
}

enum Cmd {
    Show(WebRequest),
    Hide,
}

struct Host {
    window: gtk::Window,
    webview: wry::WebView,
    css: gtk::CssProvider,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

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
        apply_chrome(host, &request);
        if let Err(err) = host.webview.load_html(&request.html) {
            eprintln!("fell: web view load failed: {err}");
        }
        tighten(&host.webview, request.allow_images);
        host.window.show_all();
        host.window.present();
    });
}

fn open_host(request: &WebRequest) -> wry::Result<Host> {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_default_size(960, 720);
    let header = gtk::HeaderBar::new();
    header.set_show_close_button(true);
    header.set_title(Some("Fell"));
    window.set_titlebar(Some(&header));
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    window.add(&vbox);
    let css = gtk::CssProvider::new();
    if let Some(screen) = gtk::gdk::Screen::default() {
        gtk::StyleContext::add_provider_for_screen(
            &screen,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
    apply_css(&css, request);
    window.show_all();
    let webview = wry::WebViewBuilder::new()
        .with_html(&request.html)
        .with_javascript_disabled()
        .with_autoplay(false)
        .with_clipboard(false)
        .with_background_color((
            request.background.0,
            request.background.1,
            request.background.2,
            255,
        ))
        .with_navigation_handler(|uri| !remote_navigation(&uri))
        .with_new_window_req_handler(|_url, _features| wry::NewWindowResponse::Deny)
        .build_gtk(&vbox)?;
    tighten(&webview, request.allow_images);
    Ok(Host {
        window,
        webview,
        css,
    })
}

fn apply_chrome(host: &Host, request: &WebRequest) {
    host.window.set_title(&request.title);
    if let Some(bar) = host
        .window
        .titlebar()
        .and_then(|widget| widget.downcast::<gtk::HeaderBar>().ok())
    {
        bar.set_subtitle(Some(&request.title));
    }
    apply_css(&host.css, request);
}

fn apply_css(css: &gtk::CssProvider, request: &WebRequest) {
    let (br, bg, bb) = request.background;
    let (fr, fg, fb) = request.foreground;
    let data = format!(
        "window, headerbar {{ background-color: #{br:02x}{bg:02x}{bb:02x}; }} \
         headerbar .title, headerbar .subtitle, headerbar label {{ color: #{fr:02x}{fg:02x}{fb:02x}; }}"
    );
    let _ = css.load_from_data(data.as_bytes());
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
