//! WebKitGTK via wry, painted into the conversation.
//!
//! wry 0.57 can child-embed only into an Xlib window, and GPUI on Wayland
//! does not provide one. The newsletter is laid out in a GTK offscreen
//! window, which Wayland does not map, then snapshotted with JavaScript
//! still disabled and drawn in the message body. The snapshot waits until
//! the document and its images have finished; a load that never finishes
//! becomes a terminal failure instead of an endless poll. There is no title
//! bar and no second copy of the subject. The library stays dynamically linked.

use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use gtk::prelude::*;
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
    /// Bumped on every load. A snapshot callback from the previous load
    /// no longer matches.
    capture: Cell<u64>,
    /// Bumped whenever a snapshot is scheduled or abandoned, so a picture
    /// taken while images were still loading cannot be stored.
    shot: Cell<u64>,
    progress: Cell<LoadProgress>,
    pending: RefCell<Option<gtk::glib::SourceId>>,
    fail_safe: RefCell<Option<gtk::glib::SourceId>>,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

struct StoredPng {
    bytes: Arc<[u8]>,
    generation: u64,
    settled: bool,
}

enum StoredFrame {
    Png(StoredPng),
    Failed,
}

static FRAME: Mutex<Option<(u64, StoredFrame)>> = Mutex::new(None);

pub enum WebStatus {
    Pending,
    Ready {
        png: Arc<[u8]>,
        generation: u64,
        settled: bool,
    },
    /// The reader already holds this generation, so the PNG is not cloned.
    Unchanged {
        settled: bool,
    },
    Failed,
}

pub fn show(request: WebRequest) {
    match boot() {
        WebTransport::Live(tx) => {
            let token = request.token;
            if tx.send(Cmd::Show(request)).is_err() {
                publish_failed(token);
            }
        }
        WebTransport::Failed => publish_failed(request.token),
    }
}

pub fn hide() {
    if let WebTransport::Live(tx) = boot() {
        let _ = tx.send(Cmd::Hide);
    }
}

/// Latest full-document PNG for `token`, or a terminal failure.
///
/// `Pending` means the load is still settling. The reader polls this on its
/// own; it does not rebuild the HTML to find out. Pass the generation already
/// on screen so an unchanged frame is not cloned.
pub fn status_for(token: u64, seen_generation: Option<u64>) -> WebStatus {
    let Ok(slot) = FRAME.lock() else {
        return WebStatus::Pending;
    };
    match slot.as_ref() {
        Some((got, StoredFrame::Png(png))) if *got == token => {
            if seen_generation == Some(png.generation) {
                WebStatus::Unchanged {
                    settled: png.settled,
                }
            } else {
                WebStatus::Ready {
                    png: Arc::clone(&png.bytes),
                    generation: png.generation,
                    settled: png.settled,
                }
            }
        }
        Some((got, StoredFrame::Failed)) if *got == token => WebStatus::Failed,
        _ => WebStatus::Pending,
    }
}

enum WebTransport {
    Live(gtk::glib::Sender<Cmd>),
    Failed,
}

static SENDER: OnceLock<WebTransport> = OnceLock::new();

fn boot() -> &'static WebTransport {
    SENDER.get_or_init(|| {
        let (ready_tx, ready_rx) = mpsc::channel();
        if std::thread::Builder::new()
            .name("fell-webview".into())
            .spawn(move || gtk_thread(ready_tx))
            .is_err()
        {
            return WebTransport::Failed;
        }
        // gtk::init failure returns without sending, which closes this channel.
        // That is a failed web view, not a panic on the message window.
        match ready_rx.recv() {
            Ok(tx) => WebTransport::Live(tx),
            Err(_) => WebTransport::Failed,
        }
    })
}

fn publish_failed(token: u64) {
    if let Ok(mut frame) = FRAME.lock() {
        *frame = Some((token, StoredFrame::Failed));
    }
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
    // The returned id removes its source when dropped. It has to outlive
    // gtk::main, or Show and Hide are never delivered.
    let _source = rx.attach(None, |cmd| {
        match cmd {
            Cmd::Show(request) => present(request),
            Cmd::Hide => HOST.with(|slot| {
                if let Some(host) = slot.borrow().as_ref() {
                    cancel_source(&host.pending);
                    cancel_source(&host.fail_safe);
                    host.capture.set(host.capture.get().wrapping_add(1));
                    host.shot.set(host.shot.get().wrapping_add(1));
                    let widget = host.webview.webview();
                    webkit2gtk::WebViewExt::stop_loading(&widget);
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
                    publish_failed(request.token);
                    return None;
                }
            }
        }
        let host = slot.as_mut()?;
        cancel_source(&host.pending);
        cancel_source(&host.fail_safe);
        host.token.set(request.token);
        host.capture.set(host.capture.get().wrapping_add(1));
        host.shot.set(host.shot.get().wrapping_add(1));
        host.progress.set(LoadProgress::new());
        Some(host.capture.get())
    });
    let Some(capture) = created else {
        return;
    };
    let (window, widget) = HOST.with(|slot| {
        let slot = slot.borrow();
        let host = slot.as_ref().expect("webview host");
        (host.window.clone(), host.webview.webview())
    });
    window.show_all();
    webkit2gtk::WebViewExt::load_html(&widget, &request.html, None);
    tighten_widget(&widget, request.allow_images);
    arm_fail_safe(request.token, capture);
}

fn open_host(request: &WebRequest) -> wry::Result<Host> {
    let window = gtk::OffscreenWindow::new();
    window.set_focus_on_map(false);
    window.set_accept_focus(false);
    window.set_can_focus(false);
    // Match the message pane so the snapshot is not scaled, which softens type.
    window.set_default_size(928, 1400);
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    window.add(&vbox);
    window.show_all();
    let (red, green, blue) = request.background;
    let webview = wry::WebViewBuilder::new()
        .with_html("<!DOCTYPE html><html><body></body></html>")
        .with_javascript_disabled()
        .with_autoplay(false)
        .with_clipboard(false)
        .with_background_color((red, green, blue, 255))
        .with_navigation_handler(|uri| !remote_navigation(&uri))
        .with_new_window_req_handler(|_url, _features| wry::NewWindowResponse::Deny)
        .build_gtk(&vbox)?;
    tighten(&webview, request.allow_images);
    install_proxy(&webview.webview())?;
    watch_loads(&webview.webview());
    Ok(Host {
        window,
        webview,
        token: Cell::new(request.token),
        capture: Cell::new(0),
        shot: Cell::new(0),
        progress: Cell::new(LoadProgress::new()),
        pending: RefCell::new(None),
        fail_safe: RefCell::new(None),
    })
}

fn install_proxy(widget: &webkit2gtk::WebView) -> wry::Result<()> {
    use gtk::glib::translate::{IntoGlib, ToGlibPtr, ToGlibPtrMut};
    use webkit2gtk::{NetworkProxyMode, WebViewExt};
    let port = super::proxy::port();
    let uri = format!("http://127.0.0.1:{port}");
    let mut settings = webkit2gtk::NetworkProxySettings::new(Some(&uri), &[]);
    let Some(context) = widget.context() else {
        return Err(wry::Error::InitScriptError);
    };
    // WebContextExtManual is not re-exported by webkit2gtk 2.0.2.
    unsafe {
        webkit2gtk::ffi::webkit_web_context_set_network_proxy_settings(
            context.to_glib_none().0,
            NetworkProxyMode::Custom.into_glib(),
            settings.to_glib_none_mut().0,
        );
    }
    Ok(())
}

fn watch_loads(widget: &webkit2gtk::WebView) {
    use webkit2gtk::{LoadEvent, WebViewExt};
    widget.connect_load_changed(|_, event| {
        if event == LoadEvent::Finished {
            apply_progress(LoadProgress::document_finished);
        }
    });
    widget.connect_load_failed(|_, _, _, err| {
        if !load_was_cancelled(err) {
            apply_progress(LoadProgress::timed_out);
        }
        false
    });
    widget.connect_web_process_terminated(|_, _| {
        apply_progress(LoadProgress::timed_out);
    });
    widget.connect_decide_policy(|_, decision, kind| deny_blocked_request(decision, kind));
    widget.connect_resource_load_started(|_, resource, _| track_resource(resource));
}

fn deny_blocked_request(
    decision: &webkit2gtk::PolicyDecision,
    kind: webkit2gtk::PolicyDecisionType,
) -> bool {
    use webkit2gtk::{
        NavigationPolicyDecisionExt, PolicyDecisionExt, PolicyDecisionType,
        ResponsePolicyDecisionExt, URIRequestExt, URIResponseExt,
    };
    let uri = match kind {
        PolicyDecisionType::NavigationAction | PolicyDecisionType::NewWindowAction => decision
            .clone()
            .downcast::<webkit2gtk::NavigationPolicyDecision>()
            .ok()
            .and_then(|decision| decision.navigation_action())
            .and_then(|action| action.request())
            .and_then(|request| request.uri()),
        PolicyDecisionType::Response => decision
            .clone()
            .downcast::<webkit2gtk::ResponsePolicyDecision>()
            .ok()
            .and_then(|decision| {
                decision
                    .request()
                    .and_then(|request| request.uri())
                    .or_else(|| decision.response().and_then(|response| response.uri()))
            }),
        PolicyDecisionType::__Unknown(_) | _ => None,
    };
    if uri.as_deref().is_some_and(fell_render::request_is_blocked) {
        decision.ignore();
        return true;
    }
    false
}

fn track_resource(resource: &webkit2gtk::WebResource) {
    use webkit2gtk::WebResourceExt;
    apply_progress(LoadProgress::start_resource);
    let Some(capture) = current_capture() else {
        return;
    };
    let done = Rc::new(Cell::new(false));
    resource.connect_finished({
        let done = Rc::clone(&done);
        move |_| finish_resource(&done, capture)
    });
    resource.connect_failed({
        let done = Rc::clone(&done);
        move |_, _| finish_resource(&done, capture)
    });
    resource.connect_failed_with_tls_errors({
        let done = Rc::clone(&done);
        move |_, _, _| finish_resource(&done, capture)
    });
}

fn finish_resource(done: &Rc<Cell<bool>>, capture: u64) {
    if done.replace(true) || current_capture() != Some(capture) {
        return;
    }
    apply_progress(LoadProgress::end_resource);
}

fn current_capture() -> Option<u64> {
    HOST.with(|slot| slot.borrow().as_ref().map(|host| host.capture.get()))
}

fn load_was_cancelled(err: &webkit2gtk::Error) -> bool {
    err.matches(webkit2gtk::NetworkError::Cancelled)
        || err.message().to_ascii_lowercase().contains("cancel")
}

fn apply_progress(update: impl FnOnce(&mut LoadProgress) -> Settle) {
    let Some((token, capture, action)) = HOST.with(|slot| {
        let slot = slot.borrow();
        let host = slot.as_ref()?;
        let mut progress = host.progress.get();
        let action = update(&mut progress);
        host.progress.set(progress);
        Some((host.token.get(), host.capture.get(), action))
    }) else {
        return;
    };
    match action {
        Settle::Wait => {}
        Settle::Snapshot { delay_ms } => {
            schedule_snapshot(token, capture, Duration::from_millis(delay_ms));
        }
        Settle::Failed => publish_failure(token, capture),
        Settle::Finish => mark_settled(token, capture),
    }
}

fn schedule_snapshot(token: u64, capture: u64, delay: Duration) {
    let Some((widget, shot)) = HOST.with(|slot| {
        let slot = slot.borrow();
        let host = slot.as_ref()?;
        let progress = host.progress.get();
        if host.token.get() != token || host.capture.get() != capture || progress.settled {
            return None;
        }
        cancel_source(&host.pending);
        let shot = host.shot.get().wrapping_add(1);
        host.shot.set(shot);
        Some((host.webview.webview(), shot))
    }) else {
        return;
    };
    let id = gtk::glib::timeout_add_local(delay, move || {
        let still = HOST.with(|slot| {
            slot.borrow().as_ref().is_some_and(|host| {
                let progress = host.progress.get();
                host.token.get() == token
                    && host.capture.get() == capture
                    && host.shot.get() == shot
                    && !progress.settled
                    && progress.document_done
            })
        });
        if still {
            HOST.with(|slot| {
                if let Some(host) = slot.borrow().as_ref()
                    && let Some(id) = host.pending.borrow_mut().take()
                {
                    // Break retires this source. Forget the id so a Drop
                    // that calls remove cannot retire it a second time.
                    #[allow(clippy::forget_non_drop)]
                    std::mem::forget(id);
                }
            });
            snapshot(widget.clone(), token, capture, shot);
        }
        gtk::glib::ControlFlow::Break
    });
    HOST.with(|slot| {
        if let Some(host) = slot.borrow().as_ref() {
            if host.token.get() == token && host.capture.get() == capture && host.shot.get() == shot
            {
                *host.pending.borrow_mut() = Some(id);
            } else {
                id.remove();
            }
        } else {
            id.remove();
        }
    });
}

fn snapshot(widget: webkit2gtk::WebView, token: u64, capture: u64, shot: u64) {
    use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt};
    widget.snapshot(
        SnapshotRegion::FullDocument,
        SnapshotOptions::NONE,
        None::<&gtk::gio::Cancellable>,
        move |result| {
            let current = HOST.with(|slot| {
                slot.borrow().as_ref().is_some_and(|host| {
                    let progress = host.progress.get();
                    host.token.get() == token
                        && host.capture.get() == capture
                        && host.shot.get() == shot
                        && !progress.settled
                        && progress.document_done
                })
            });
            if !current {
                return;
            }
            let settled = HOST.with(|slot| {
                slot.borrow().as_ref().is_some_and(|host| {
                    let progress = host.progress.get();
                    progress.document_done && progress.inflight == 0
                })
            });
            if let Ok(surface) = result
                && let Some(png) = surface_png(surface)
            {
                let generation = HOST.with(|slot| {
                    slot.borrow().as_ref().map(|host| {
                        let generation = host.shot.get();
                        let mut progress = host.progress.get();
                        progress.note_snapshot(settled);
                        host.progress.set(progress);
                        if settled {
                            cancel_source(&host.fail_safe);
                            host.window.hide();
                        }
                        generation
                    })
                });
                if let Some(generation) = generation
                    && let Ok(mut frame) = FRAME.lock()
                {
                    *frame = Some((
                        token,
                        StoredFrame::Png(StoredPng {
                            bytes: Arc::from(png),
                            generation,
                            settled,
                        }),
                    ));
                }
                return;
            }
            apply_progress(LoadProgress::snapshot_empty);
        },
    );
}

fn mark_settled(token: u64, capture: u64) {
    let still = HOST.with(|slot| {
        slot.borrow().as_ref().is_some_and(|host| {
            host.token.get() == token
                && host.capture.get() == capture
                && host.progress.get().settled
        })
    });
    if !still {
        return;
    }
    if let Ok(mut frame) = FRAME.lock()
        && let Some((got, StoredFrame::Png(png))) = frame.as_mut()
        && *got == token
    {
        png.settled = true;
    }
    HOST.with(|slot| {
        if let Some(host) = slot.borrow().as_ref()
            && host.token.get() == token
            && host.capture.get() == capture
        {
            cancel_source(&host.pending);
            cancel_source(&host.fail_safe);
            host.window.hide();
        }
    });
}

fn publish_failure(token: u64, capture: u64) {
    let still = HOST.with(|slot| {
        slot.borrow().as_ref().is_some_and(|host| {
            host.token.get() == token
                && host.capture.get() == capture
                && host.progress.get().published
        })
    });
    if !still {
        return;
    }
    if let Ok(mut frame) = FRAME.lock() {
        *frame = Some((token, StoredFrame::Failed));
    }
    HOST.with(|slot| {
        if let Some(host) = slot.borrow().as_ref()
            && host.token.get() == token
            && host.capture.get() == capture
        {
            cancel_source(&host.pending);
            cancel_source(&host.fail_safe);
            host.window.hide();
        }
    });
}

fn arm_fail_safe(token: u64, capture: u64) {
    let id = gtk::glib::timeout_add_local(Duration::from_secs(8), move || {
        // Take the id before GLib retires this source. A later Show must not
        // call remove on a source that has already finished.
        HOST.with(|slot| {
            if let Some(host) = slot.borrow().as_ref()
                && let Some(id) = host.fail_safe.borrow_mut().take()
            {
                #[allow(clippy::forget_non_drop)]
                std::mem::forget(id);
            }
        });
        let should_stop = HOST.with(|slot| {
            slot.borrow().as_ref().is_some_and(|host| {
                host.token.get() == token
                    && host.capture.get() == capture
                    && !host.progress.get().settled
            })
        });
        if should_stop {
            apply_progress(LoadProgress::timed_out);
        }
        gtk::glib::ControlFlow::Break
    });
    HOST.with(|slot| {
        if let Some(host) = slot.borrow().as_ref() {
            if host.token.get() == token && host.capture.get() == capture {
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
    let Ok(width) = u32::try_from(width) else {
        return None;
    };
    let Ok(height) = u32::try_from(height) else {
        return None;
    };
    let (dst_w, dst_h) = super::native::fit_snapshot(width, height);
    let stride = image.stride() as usize;
    let data = image.data().ok()?;
    let src_w = width as usize;
    let src_h = height as usize;
    let dst_w_us = dst_w as usize;
    let dst_h_us = dst_h as usize;
    let mut rgba = Vec::with_capacity(dst_w_us * dst_h_us * 4);
    for y in 0..dst_h_us {
        let src_y = y * src_h / dst_h_us;
        for x in 0..dst_w_us {
            let src_x = x * src_w / dst_w_us;
            let offset = src_y * stride + src_x * 4;
            let px = data.get(offset..offset + 4)?;
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
    let dst_h = super::native::trim_uniform_bottom(&rgba, dst_w, dst_h);
    rgba.truncate(dst_w as usize * dst_h as usize * 4);
    let buffer = image::RgbaImage::from_raw(dst_w, dst_h, rgba)?;
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
    if fell_render::request_is_blocked(uri) || fell_render::targets_private_network(uri) {
        return true;
    }
    let uri = uri.to_ascii_lowercase();
    uri.starts_with("http://") || uri.starts_with("https://") || uri.starts_with("file:")
}

pub fn ensure_started() {
    boot();
}

/// When a web-view load may be snapshotted.
///
/// The document finishing schedules a picture immediately. If images were
/// still loading, another picture is scheduled once they settle, and that
/// one replaces the first. An empty picture is retried, then reported as
/// failure. A load that never produces a frame fails on the timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settle {
    Wait,
    Snapshot {
        delay_ms: u64,
    },
    Failed,
    /// Keep the picture we have and stop waiting.
    Finish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LoadProgress {
    inflight: u32,
    document_done: bool,
    published: bool,
    settled: bool,
    empty_snapshots: u32,
}

impl LoadProgress {
    fn new() -> Self {
        Self {
            inflight: 0,
            document_done: false,
            published: false,
            settled: false,
            empty_snapshots: 0,
        }
    }

    fn start_resource(&mut self) -> Settle {
        self.inflight = self.inflight.saturating_add(1);
        Settle::Wait
    }

    fn end_resource(&mut self) -> Settle {
        self.inflight = self.inflight.saturating_sub(1);
        if self.document_done && self.inflight == 0 && !self.settled {
            Settle::Snapshot { delay_ms: 120 }
        } else {
            Settle::Wait
        }
    }

    fn document_finished(&mut self) -> Settle {
        self.document_done = true;
        if self.settled {
            Settle::Wait
        } else {
            Settle::Snapshot { delay_ms: 120 }
        }
    }

    fn note_snapshot(&mut self, settled: bool) {
        self.published = true;
        self.empty_snapshots = 0;
        if settled {
            self.settled = true;
        }
    }

    fn snapshot_empty(&mut self) -> Settle {
        if self.settled {
            return Settle::Wait;
        }
        self.empty_snapshots = self.empty_snapshots.saturating_add(1);
        if self.empty_snapshots < 4 {
            return Settle::Snapshot {
                delay_ms: 200 * u64::from(self.empty_snapshots),
            };
        }
        self.settled = true;
        if self.published {
            Settle::Finish
        } else {
            self.published = true;
            Settle::Failed
        }
    }

    fn timed_out(&mut self) -> Settle {
        if self.settled {
            Settle::Wait
        } else if self.published {
            self.settled = true;
            Settle::Finish
        } else {
            self.published = true;
            self.settled = true;
            Settle::Failed
        }
    }
}

#[cfg(test)]
mod settle_tests {
    use super::{LoadProgress, Settle};

    #[test]
    fn snapshot_again_after_images_finish() {
        let mut load = LoadProgress::new();
        assert_eq!(load.start_resource(), Settle::Wait);
        assert_eq!(load.start_resource(), Settle::Wait);
        assert_eq!(load.document_finished(), Settle::Snapshot { delay_ms: 120 });
        load.note_snapshot(false);
        assert_eq!(load.end_resource(), Settle::Wait);
        assert_eq!(
            load.end_resource(),
            Settle::Snapshot { delay_ms: 120 },
            "replace the early picture once every image has finished"
        );
        load.note_snapshot(true);
        assert_eq!(load.end_resource(), Settle::Wait);
        assert_eq!(load.timed_out(), Settle::Wait);
    }

    #[test]
    fn timeout_without_a_frame_fails() {
        let mut load = LoadProgress::new();
        assert_eq!(load.timed_out(), Settle::Failed);
        assert_eq!(load.timed_out(), Settle::Wait);
        assert_eq!(load.document_finished(), Settle::Wait);

        let mut early = LoadProgress::new();
        early.document_finished();
        early.note_snapshot(false);
        assert_eq!(early.timed_out(), Settle::Finish);
        assert_eq!(early.timed_out(), Settle::Wait);
    }

    #[test]
    fn empty_snapshots_stop_instead_of_polling() {
        let mut retry = LoadProgress::new();
        retry.document_finished();
        assert_eq!(retry.snapshot_empty(), Settle::Snapshot { delay_ms: 200 });
        assert_eq!(retry.snapshot_empty(), Settle::Snapshot { delay_ms: 400 });
        assert_eq!(retry.snapshot_empty(), Settle::Snapshot { delay_ms: 600 });
        assert_eq!(retry.snapshot_empty(), Settle::Failed);
        assert_eq!(retry.snapshot_empty(), Settle::Wait);
    }
}

#[cfg(test)]
mod frame_tests {
    use std::sync::Arc;

    use super::{FRAME, StoredFrame, StoredPng, WebStatus, status_for};

    #[test]
    fn unchanged_generation_does_not_clone_the_png() {
        let bytes: Arc<[u8]> = Arc::from(&b"not-a-real-png"[..]);
        let token = 9_001_001_u64;
        {
            let mut slot = FRAME.lock().expect("frame lock");
            *slot = Some((
                token,
                StoredFrame::Png(StoredPng {
                    bytes: Arc::clone(&bytes),
                    generation: 4,
                    settled: false,
                }),
            ));
        }
        let WebStatus::Ready {
            png,
            generation,
            settled,
        } = status_for(token, None)
        else {
            panic!("first poll should hand over the arc");
        };
        assert_eq!(generation, 4);
        assert!(!settled);
        assert!(Arc::ptr_eq(&png, &bytes));
        let holders = Arc::strong_count(&bytes);
        match status_for(token, Some(4)) {
            WebStatus::Unchanged { settled } => assert!(!settled),
            WebStatus::Pending => panic!("expected no copy, got pending"),
            WebStatus::Ready { .. } => panic!("expected no copy, got another png"),
            WebStatus::Failed => panic!("expected no copy, got failure"),
        }
        assert_eq!(Arc::strong_count(&bytes), holders);
        drop(png);
        if let Ok(mut slot) = FRAME.lock()
            && slot.as_ref().is_some_and(|(got, _)| *got == token)
        {
            *slot = None;
        }
    }
}

#[cfg(test)]
mod fetch_proof {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use gtk::prelude::*;
    use wry::WebViewExtUnix;

    use super::{WebRequest, open_host};

    /// Remote images are allowed. The mail names 127.0.0.1 from an img, a
    /// comment-wrapped CSS url(), an @import, and a font, and it also names a
    /// public-looking host that redirects there. Neither server is allowed to
    /// see the private target.
    #[test]
    fn private_targets_receive_no_requests() {
        if std::env::var_os("DISPLAY").is_none() {
            eprintln!("skip private_targets_receive_no_requests: no DISPLAY");
            return;
        }
        let _ = std::process::Command::new("sudo")
            .args(["/usr/bin/ip", "addr", "add", "192.0.2.1/32", "dev", "lo"])
            .status();
        let private_hits = Arc::new(AtomicUsize::new(0));
        let private_port = serve_private(Arc::clone(&private_hits));
        let redirect_hits = Arc::new(AtomicUsize::new(0));
        let redirect_port = serve_redirect(private_port, Arc::clone(&redirect_hits));
        let html = format!(
            r#"<!DOCTYPE html><html><head><style>
@import url("http://127.0.0.1:{private_port}/import.css");
@font-face {{ font-family: Probe; src: url("http://127.0.0.1:{private_port}/font.woff"); }}
body {{ background-image: url(/*x*/"http://127.0.0.1:{private_port}/bg.png"); }}
</style></head><body>
<img src="http://127.0.0.1:{private_port}/img.png">
<img src="http://192.0.2.1:{redirect_port}/go">
</body></html>"#
        );
        gtk::init().expect("gtk");
        let host = open_host(&WebRequest {
            html: html.clone(),
            token: 1,
            background: (255, 255, 255),
            allow_images: true,
        })
        .expect("webview");
        host.window.show_all();
        let widget = host.webview.webview();
        webkit2gtk::WebViewExt::load_html(&widget, &html, None);
        super::tighten_widget(&widget, true);
        let _timer = gtk::glib::timeout_add_local(Duration::from_secs(4), || {
            gtk::main_quit();
            gtk::glib::ControlFlow::Break
        });
        gtk::main();
        let private = private_hits.load(Ordering::SeqCst);
        let redirect = redirect_hits.load(Ordering::SeqCst);
        eprintln!("proof private_hits={private} redirect_hits={redirect}");
        assert_eq!(private, 0, "127.0.0.1 received a request");
        assert!(
            redirect > 0,
            "the public-looking redirect host was never asked"
        );
    }

    fn serve_private(hits: Arc<AtomicUsize>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                hits.fetch_add(1, Ordering::SeqCst);
                let mut conn = conn;
                let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        port
    }

    fn serve_redirect(private_port: u16, hits: Arc<AtomicUsize>) -> u16 {
        let listener = TcpListener::bind("192.0.2.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                hits.fetch_add(1, Ordering::SeqCst);
                let mut conn = conn;
                let mut buf = [0u8; 1024];
                let _ = conn.read(&mut buf);
                let body = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{private_port}/from-redirect\r\nContent-Length: 0\r\n\r\n"
                );
                let _ = conn.write_all(body.as_bytes());
            }
        });
        port
    }
}
