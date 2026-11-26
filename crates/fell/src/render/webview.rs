//! One reusable WebKitGTK view on a GTK thread. Mail is snapshotted offscreen
//! so GPUI can keep its own focus, scrolling, and link activation.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::channel::oneshot;
use gpui::RenderImage;
use gtk::glib;
use gtk::prelude::*;
use javascriptcore::ValueExt;
use webkit2gtk::{
    PolicyDecisionExt, SettingsExt, UserContentInjectedFrames, UserContentManagerExt,
    UserStyleLevel, UserStyleSheet, WebViewExt,
};
use wry::{WebViewBuilderExtUnix, WebViewExtUnix};

const UNLAID_HEIGHT: i32 = 1;
pub(crate) const SNAPSHOT_HEIGHT_LIMIT: i32 = super::native::MAX_IMAGE_EDGE as i32;
const PAINT_SETTLE: Duration = Duration::from_millis(80);
pub(crate) const LARGE_HTML_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub(crate) struct Snapshot {
    pub image: Arc<RenderImage>,
    pub width: f32,
    pub height: f32,
    pub clipped: bool,
    pub document_height: f32,
    pub last_row_color: u32,
}

pub(crate) struct WebRequest {
    pub html: String,
    pub width: i32,
    pub background: u32,
}

impl WebRequest {
    fn load_budget(&self) -> Duration {
        Duration::from_secs(if self.html.len() > LARGE_HTML_BYTES {
            12
        } else {
            4
        })
    }
}

struct Job {
    request: WebRequest,
    reply: Reply,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Superseded;

pub(crate) type RenderResult = Result<Option<Snapshot>, Superseded>;
type Reply = oneshot::Sender<RenderResult>;

#[derive(Default)]
struct JobQueue(Mutex<Option<Job>>);

impl JobQueue {
    fn submit(&self, request: WebRequest) -> oneshot::Receiver<RenderResult> {
        let (reply, done) = oneshot::channel();
        let superseded = self.0.lock().unwrap().replace(Job { request, reply });
        if let Some(job) = superseded {
            let _ = job.reply.send(Err(Superseded));
        }
        done
    }

    fn next(&self) -> Option<Job> {
        self.0
            .lock()
            .unwrap()
            .take()
            .filter(|job| !job.reply.is_canceled())
    }
}

#[allow(deprecated)]
struct Worker {
    queue: Arc<JobQueue>,
    wake: glib::Sender<()>,
}

struct Load {
    reply: Reply,
    cancel: gtk::gio::Cancellable,
    settling: bool,
    background: u32,
}

impl Load {
    fn new(reply: Reply, background: u32) -> Self {
        Self {
            reply,
            cancel: gtk::gio::Cancellable::new(),
            settling: false,
            background,
        }
    }
}

type Active = Rc<RefCell<Option<Load>>>;

#[allow(deprecated)]
fn worker() -> Option<&'static Worker> {
    static WORKER: OnceLock<Option<Worker>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            let (wake, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
            let queue = Arc::new(JobQueue::default());
            let queued = queue.clone();
            std::thread::Builder::new()
                .name("fell-webview".into())
                .spawn(move || {
                    if let Err(error) = run(rx, queued) {
                        #[cfg(test)]
                        eprintln!("WebKit initialization: {error}");
                        fell_core::diagnostic!(
                            WARN,
                            "Could not initialize the mail web view",
                            error = &*error
                        );
                    }
                })
                .ok()?;
            Some(Worker { queue, wake })
        })
        .as_ref()
}

pub(crate) fn prewarm() {
    worker();
}

pub(crate) async fn render(request: WebRequest) -> RenderResult {
    let Some(worker) = worker() else {
        return Ok(None);
    };
    let budget = request.load_budget();
    let done = worker.queue.submit(request);
    if worker.wake.send(()).is_err() {
        return Ok(None);
    }
    // The worker can fail during setup, before GLib attaches its receiver.
    // Bound the wait even when GLib keeps an already queued request alive.
    let timeout = async_io::Timer::after(budget + Duration::from_secs(2));
    match futures::future::select(done, timeout).await {
        futures::future::Either::Left((result, _)) => result.unwrap_or(Ok(None)),
        futures::future::Either::Right(_) => Ok(None),
    }
}

#[allow(deprecated)]
fn run(rx: glib::Receiver<()>, queue: Arc<JobQueue>) -> Result<(), Box<dyn std::error::Error>> {
    gtk::init()?;
    let window = gtk::OffscreenWindow::new();
    window.set_accept_focus(false);
    window.set_default_size(1, UNLAID_HEIGHT);
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    window.add(&container);
    let port = super::proxy::port().ok_or("could not start the WebKit deny proxy")?;
    let webview = wry::WebViewBuilder::new()
        .with_proxy_config(wry::ProxyConfig::Http(wry::ProxyEndpoint {
            host: "127.0.0.1".into(),
            port: port.to_string(),
        }))
        .with_incognito(true)
        // Leave Wry's opt-in Allow policy off. secure() supplies Deny for each load.
        .with_autoplay(false)
        .with_clipboard(false)
        .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
        .build_gtk(&container)?;
    let widget = webview.webview();
    secure(&widget)?;
    let styles = widget
        .user_content_manager()
        .ok_or("missing WebKit content manager")?;
    let active: Active = Rc::default();
    widget.connect_load_changed({
        let active = active.clone();
        move |widget, event| {
            if event == webkit2gtk::LoadEvent::Finished {
                schedule_capture(widget, &active);
            }
        }
    });
    widget.connect_web_process_terminated({
        let active = active.clone();
        move |_, _| stop_load(&active, Ok(None))
    });
    window.show_all();
    // Prepare GTK and the content filter during prewarm. Let the first job
    // drive navigation so it cannot supersede an in-flight warmup document.
    let _source = rx.attach(None, move |()| {
        let Some(job) = queue.next() else {
            return glib::ControlFlow::Continue;
        };
        stop_load(&active, Err(Superseded));
        widget.stop_loading();
        let budget = job.request.load_budget();
        let WebRequest {
            html,
            width,
            background,
        } = job.request;
        widget.set_size_request(width, 1);
        window.resize(width, UNLAID_HEIGHT);
        styles.remove_all_style_sheets();
        let image_limit = fell_render::WebMail::image_height_limit(width as f32);
        styles.add_style_sheet(&UserStyleSheet::new(
            &format!(
                ":root {{ background-color: #{background:06x}; }}
                 img {{ min-height: 0 !important; max-height: {image_limit}px !important; }}
                 html::-webkit-scrollbar {{ display: none; }}"
            ),
            UserContentInjectedFrames::TopFrame,
            UserStyleLevel::User,
            &[],
            &[],
        ));
        let channel = |shift| f64::from((background >> shift) & 255_u32) / 255.0;
        widget.set_background_color(&gtk::gdk::RGBA::new(
            channel(16),
            channel(8),
            channel(0),
            1.0,
        ));
        let load = Load::new(job.reply, background);
        let cancellable = load.cancel.clone();
        *active.borrow_mut() = Some(load);
        widget.load_html(&html, None);
        glib::timeout_add_local_once(budget, {
            let active = active.clone();
            let widget = widget.downgrade();
            move || {
                if active
                    .borrow()
                    .as_ref()
                    .is_some_and(|load| load.cancel == cancellable)
                {
                    stop_load(&active, Ok(None));
                    if let Some(widget) = widget.upgrade() {
                        widget.stop_loading();
                    }
                }
            }
        });
        glib::ControlFlow::Continue
    });
    gtk::main();
    Ok(())
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestWorker {
    queue: JobQueue,
    active: Active,
}

#[cfg(test)]
impl TestWorker {
    pub(crate) async fn render(&self, request: WebRequest) -> RenderResult {
        self.queue.submit(request).await.unwrap()
    }

    pub(crate) fn start_next(&self) {
        let job = self.queue.next().expect("queued snapshot");
        stop_load(&self.active, Err(Superseded));
        *self.active.borrow_mut() = Some(Load::new(job.reply, job.request.background));
    }

    pub(crate) fn finish(&self, snapshot: Snapshot) {
        let load = self.active.borrow_mut().take().expect("in-flight snapshot");
        assert!(load.reply.send(Ok(Some(snapshot))).is_ok());
    }
}

fn stop_load(active: &Active, result: RenderResult) {
    let old = active.borrow_mut().take();
    if let Some(load) = old {
        load.cancel.cancel();
        let _ = load.reply.send(result);
    }
}

fn schedule_capture(widget: &webkit2gtk::WebView, active: &Active) {
    let token = {
        let mut current = active.borrow_mut();
        let Some(load) = current.as_mut().filter(|load| !load.settling) else {
            return;
        };
        load.settling = true;
        load.cancel.clone()
    };
    wait_for_images(widget, active.clone(), token);
}

#[allow(deprecated)] // run_javascript_in_world is available on WebKitGTK 2.38.
fn wait_for_images(widget: &webkit2gtk::WebView, active: Active, token: gtk::gio::Cancellable) {
    // Offscreen documents can leave img.decode() promises pending until a
    // visible paint. Wait for resource completion and force rasterization
    // with a canvas read before giving the document a chance to paint.
    const READY: &str = r#"
        (() => {
            const all = Array.from(document.images);
            const images = all.filter(image =>
                /^data:image\/(?:png|jpe?g|gif|webp|avif|bmp|x-icon|vnd\.microsoft\.icon)[;,]/i
                    .test(image.currentSrc || image.getAttribute('src') || ''));
            for (const image of images) {
                if (image.loading === 'lazy') image.loading = 'eager';
            }
            if (images.some(image => !image.complete)) return 0;
            for (const image of images) {
                // Use the same placeholder for missing and damaged bytes.
                if (image.naturalWidth === 0) image.setAttribute('src', '');
            }
            for (const image of all) {
                if (image.getAttribute('src') !== '') continue;
                const ratio = getComputedStyle(image).aspectRatio;
                if (ratio.startsWith('auto ')) image.style.aspectRatio = ratio.slice(5);
            }
            if (images.length > 0) {
                const canvas = document.createElement('canvas');
                canvas.width = canvas.height = 1;
                const context = canvas.getContext('2d');
                for (const image of images) {
                    if (image.naturalWidth === 0) continue;
                    context.drawImage(image, 0, 0);
                    context.getImageData(0, 0, 1, 1);
                }
            }
            return Math.max(1, document.documentElement.scrollHeight, document.body.scrollHeight);
        })();
    "#;
    if token.is_cancelled() {
        return;
    }
    let view = widget.clone();
    widget.run_javascript_in_world(
        READY,
        "fell-snapshot",
        Some(&token.clone()),
        move |result| {
            if token.is_cancelled() {
                return;
            }
            let height = result.ok().and_then(|result| result.js_value());
            match height.map(|value| value.to_int32()) {
                Some(0) => {
                    glib::timeout_add_local_once(PAINT_SETTLE, move || {
                        wait_for_images(&view, active, token);
                    });
                }
                Some(height) if height > 0 => {
                    let region = if height > SNAPSHOT_HEIGHT_LIMIT
                        && let Some(window) = view.toplevel().and_downcast::<gtk::Window>()
                    {
                        // A full-document surface can exceed Cairo's size
                        // limit before we get the chance to downsample it.
                        let width = view.allocated_width();
                        view.set_size_request(width, SNAPSHOT_HEIGHT_LIMIT);
                        window.resize(width, SNAPSHOT_HEIGHT_LIMIT);
                        webkit2gtk::SnapshotRegion::Visible
                    } else {
                        webkit2gtk::SnapshotRegion::FullDocument
                    };
                    glib::timeout_add_local_once(PAINT_SETTLE, move || {
                        capture(&view, active, token, height, region, true);
                    });
                }
                _ => stop_load(&active, Ok(None)),
            }
        },
    );
}

fn capture(
    widget: &webkit2gtk::WebView,
    active: Active,
    token: gtk::gio::Cancellable,
    document_height: i32,
    region: webkit2gtk::SnapshotRegion,
    retry: bool,
) {
    if token.is_cancelled() {
        return;
    }
    let view = widget.clone();
    widget.snapshot(
        region,
        webkit2gtk::SnapshotOptions::NONE,
        Some(&token.clone()),
        move |result| {
            if token.is_cancelled() {
                return;
            }
            let clipped = region == webkit2gtk::SnapshotRegion::Visible;
            let image = active.borrow().as_ref().and_then(|load| {
                surface_image(result.ok()?, load.background, clipped, document_height)
            });
            match image {
                Some(image) => {
                    if let Some(load) = active.borrow_mut().take() {
                        let _ = load.reply.send(Ok(Some(image)));
                    }
                }
                None if retry => {
                    glib::timeout_add_local_once(PAINT_SETTLE, move || {
                        capture(&view, active, token, document_height, region, false);
                    });
                }
                None => stop_load(&active, Ok(None)),
            }
        },
    );
}

fn secure(widget: &webkit2gtk::WebView) -> Result<(), Box<dyn std::error::Error>> {
    let settings = WebViewExt::settings(widget).ok_or("missing WebKit settings")?;
    // The offscreen view hands Cairo snapshots to GPUI. Software compositing
    // avoids making the first paint depend on an EGL/DRI3 presentation path.
    settings.set_hardware_acceleration_policy(webkit2gtk::HardwareAccelerationPolicy::Never);
    // Only the fixed decode probe runs, in a separate script world. Mail
    // cannot supply scripts or handlers, and its CSP also denies scripts.
    settings.set_enable_javascript_markup(false);
    settings.set_enable_developer_extras(false);
    settings.set_enable_dns_prefetching(false);
    settings.set_enable_html5_local_storage(false);
    settings.set_enable_page_cache(false);
    settings.set_enable_webaudio(false);
    settings.set_enable_webgl(false);
    settings.set_media_playback_requires_user_gesture(true);
    let no_autoplay = webkit2gtk::WebsitePolicies::builder()
        .autoplay(webkit2gtk::AutoplayPolicy::Deny)
        .build();
    widget.connect_decide_policy(move |_, decision, kind| {
        use webkit2gtk::{
            NavigationPolicyDecisionExt, NavigationType, PolicyDecisionType, URIRequestExt,
        };
        match kind {
            PolicyDecisionType::Response => false,
            PolicyDecisionType::NavigationAction => {
                let internal = decision
                    .downcast_ref::<webkit2gtk::NavigationPolicyDecision>()
                    .and_then(|decision| decision.navigation_action())
                    .is_some_and(|action| {
                        action.navigation_type() == NavigationType::Other
                            && action.request().and_then(|req| req.uri()).as_deref()
                                == Some("about:blank")
                    });
                if internal {
                    decision.use_with_policies(&no_autoplay);
                } else {
                    decision.ignore();
                }
                true
            }
            _ => {
                decision.ignore();
                true
            }
        }
    });
    install_filter(widget)
}

fn install_filter(widget: &webkit2gtk::WebView) -> Result<(), Box<dyn std::error::Error>> {
    use glib::translate::{ToGlibPtr, from_glib_full};
    use webkit2gtk::ffi;

    const RULES: &[u8] = br#"[
      {"trigger":{"url-filter":".*"},"action":{"type":"block"}},
      {"trigger":{"url-filter":"^data:"},"action":{"type":"ignore-previous-rules"}},
      {"trigger":{"url-filter":"^cid:"},"action":{"type":"ignore-previous-rules"}},
      {"trigger":{"url-filter":"^about:blank$","resource-type":["document"]},"action":{"type":"ignore-previous-rules"}}
    ]"#;
    let dir = tempfile::tempdir()?;
    let path = std::ffi::CString::new(dir.path().as_os_str().as_encoded_bytes())?;
    let manager = widget
        .user_content_manager()
        .ok_or("missing WebKit content manager")?;
    let main = glib::MainLoop::new(None, false);
    let done = Rc::new(RefCell::new(None));
    struct Save {
        main: glib::MainLoop,
        manager: webkit2gtk::UserContentManager,
        done: Rc<RefCell<Option<Result<(), glib::Error>>>>,
    }
    unsafe extern "C" fn ready(
        source: *mut glib::gobject_ffi::GObject,
        result: *mut gtk::gio::ffi::GAsyncResult,
        data: glib::ffi::gpointer,
    ) {
        // GLib calls this once on the GTK thread. The boxed state and the
        // returned filter/error are transfer-full.
        unsafe {
            let state = Box::from_raw(data.cast::<Save>());
            let mut error = std::ptr::null_mut();
            let filter = ffi::webkit_user_content_filter_store_save_finish(
                source.cast(),
                result,
                &mut error,
            );
            *state.done.borrow_mut() = Some(if filter.is_null() {
                Err(from_glib_full(error))
            } else {
                ffi::webkit_user_content_manager_add_filter(state.manager.to_glib_none().0, filter);
                ffi::webkit_user_content_filter_unref(filter);
                Ok(())
            });
            state.main.quit();
        }
    }
    let state = Save {
        main: main.clone(),
        manager,
        done: done.clone(),
    };
    let source = glib::Bytes::from_static(RULES);
    // The store belongs to this call until the asynchronous save finishes.
    unsafe {
        let store: glib::Object =
            from_glib_full(ffi::webkit_user_content_filter_store_new(path.as_ptr()).cast());
        ffi::webkit_user_content_filter_store_save(
            store.as_ptr().cast(),
            c"mail-offline".as_ptr(),
            source.to_glib_none().0,
            std::ptr::null_mut(),
            Some(ready),
            Box::into_raw(Box::new(state)).cast(),
        );
        main.run();
    }
    done.borrow_mut()
        .take()
        .ok_or("WebKit content filter did not finish")??;
    Ok(())
}

fn surface_image(
    surface: gtk::cairo::Surface,
    background: u32,
    clipped: bool,
    document_height: i32,
) -> Option<Snapshot> {
    let mut image = gtk::cairo::ImageSurface::try_from(surface).ok()?;
    image.flush();
    let width = image.width();
    let height = image.height();
    if width <= 0 || height <= UNLAID_HEIGHT {
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
    let mut bgra = Vec::with_capacity(dst_w_us * dst_h_us * 4);
    for y in 0..dst_h_us {
        let src_y = y * src_h / dst_h_us;
        let row = data.get(src_y * stride..src_y * stride + src_w * 4)?;
        if src_w == dst_w_us {
            bgra.extend_from_slice(row);
        } else {
            for x in 0..dst_w_us {
                let offset = (x * src_w / dst_w_us) * 4;
                bgra.extend_from_slice(&row[offset..offset + 4]);
            }
        }
    }
    let (pixels, _) = bgra.as_chunks_mut::<4>();
    let first = *pixels.first()?;
    if !clipped && pixels.iter().all(|pixel| *pixel == first) {
        return None;
    }
    // Cairo stores native-endian, premultiplied ARGB32. GPUI uploads Frame's
    // backing bytes directly to a BGRA texture; the image wrapper is storage.
    for pixel in pixels {
        if cfg!(target_endian = "big") {
            pixel.reverse();
        }
        let [b, g, r, a] = pixel;
        if *a != 0 && *a != 255 {
            let alpha = u16::from(*a);
            *b = ((u16::from(*b) * 255) / alpha) as u8;
            *g = ((u16::from(*g) * 255) / alpha) as u8;
            *r = ((u16::from(*r) * 255) / alpha) as u8;
        }
    }
    let last_row_color = if clipped {
        let mut colors = std::collections::HashMap::new();
        for &[b, g, r, _] in bgra[bgra.len() - dst_w_us * 4..].as_chunks::<4>().0 {
            *colors.entry(u32::from_be_bytes([0, r, g, b])).or_insert(0) += 1;
        }
        colors
            .into_iter()
            .max_by_key(|&(color, count)| (count, color))
            .map_or(background, |(color, _)| color)
    } else {
        background
    };
    let pixels = image::RgbaImage::from_raw(dst_w, dst_h, bgra)?;
    Some(Snapshot {
        image: Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(
            pixels
        )])),
        width: dst_w as f32,
        height: dst_h as f32,
        clipped,
        document_height: document_height as f32,
        last_row_color,
    })
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::net::{TcpListener, UdpSocket};

    use super::*;
    use base64::Engine;

    fn contains_display_color(picture: &Snapshot, rgb: [u8; 3]) -> bool {
        picture
            .image
            .as_bytes(0)
            .unwrap()
            .as_chunks::<4>()
            .0
            .iter()
            .any(|&[blue, green, red, alpha]| alpha == 255 && [red, green, blue] == rgb)
    }

    fn estimate_covers_snapshot(label: &str, mail: &fell_render::WebMail, width: i32) -> Snapshot {
        let picture = futures::executor::block_on(render(WebRequest {
            html: mail.page("light", |_| None),
            width,
            background: 0xffffff,
        }))
        .unwrap()
        .unwrap_or_else(|| panic!("WebKit painted {label}"));
        let estimate = mail.estimated_height(width as f32);
        let height = picture.height * width as f32 / picture.width;
        eprintln!("{label} ({width}px): {height}px snapshot, {estimate}px estimate");
        assert!(estimate >= height, "{label}: {estimate} < {height}");
        picture
    }

    #[test]
    fn only_the_latest_queued_snapshot_is_loaded() {
        let queue = JobQueue::default();
        let [first, second, third] = ["first", "second", "third"].map(|html| {
            queue.submit(WebRequest {
                html: html.into(),
                width: 440,
                background: 0xffffff,
            })
        });
        for reply in [first, second] {
            assert!(matches!(
                futures::executor::block_on(reply),
                Ok(Err(Superseded))
            ));
        }
        assert_eq!(queue.next().unwrap().request.html, "third");
        assert!(queue.next().is_none());
        drop(third);
    }

    #[test]
    fn hostile_email_makes_zero_webkit_requests() {
        use fell_core::test_support::skip_unless_available;

        const TEST: &str = "render::webview::tests::hostile_email_makes_zero_webkit_requests";
        const CHILD: &str = "FELL_WEBKIT_CORPUS_CHILD";
        const COMPLETED: &str = "WebKit corpus passed";
        let no_display =
            std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none();
        let required = std::env::var_os("CI").is_some()
            || std::env::var_os("FELL_REQUIRE_ALL_TESTS").is_some();
        if no_display
            && !required
            && skip_unless_available(
                TEST,
                Some("WebKit requires an isolated Wayland or X11 display".into()),
            )
        {
            return;
        }

        if std::env::var_os(CHILD).is_some() {
            let passed = std::panic::catch_unwind(|| {
                assert!(!no_display, "the WebKit test child requires a display");
                check_webkit_corpus();
                eprintln!("{COMPLETED}");
            })
            .is_ok();
            // GTK and WebKit own a process-lifetime worker. WebKit's thread
            // exit handler can deadlock with its IPC threads, and C exit
            // handlers can race them. The supervisor checks the completed
            // corpus and status after the kernel closes the child process.
            // SAFETY: This branch runs only in the isolated test child,
            // after the checks have returned or their panic was reported.
            unsafe { libc::_exit(if passed { 0 } else { 101 }) }
        }

        let executable = std::env::current_exe().unwrap();
        let mut command = if no_display {
            // The Ubuntu runner ships Xvfb. Give just this child a display.
            let mut command = std::process::Command::new("xvfb-run");
            command
                .args(["-a", "--"])
                .arg(&executable)
                .env("GDK_BACKEND", "x11")
                .env("LIBGL_ALWAYS_SOFTWARE", "1")
                // Xvfb has no DRI device for the backing store's EGL probe.
                .env("WEBKIT_DISABLE_DMABUF_RENDERER", "1")
                .env_remove("DBUS_SESSION_BUS_ADDRESS");
            command
        } else {
            std::process::Command::new(executable)
        };
        let output = command
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .expect("start the isolated WebKit test child");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stderr.lines().any(|line| line == COMPLETED),
            "{}\n{stderr}",
            String::from_utf8_lossy(&output.stdout),
        );
        eprint!("{stderr}");
    }

    fn check_webkit_corpus() {
        let local_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        // UDP connect selects a local interface without sending a datagram.
        // A second loopback address still gives us a distinct origin on an
        // isolated machine with no non-loopback interface or default route.
        let public_listener = TcpListener::bind("192.0.2.1:0")
            .or_else(|_| {
                let route = UdpSocket::bind("0.0.0.0:0")?;
                route.connect("192.0.2.1:80")?;
                TcpListener::bind((route.local_addr()?.ip(), 0))
            })
            .or_else(|_| TcpListener::bind("127.0.0.2:0"))
            .expect("bind a second origin for the WebKit probe");
        for listener in [&local_listener, &public_listener] {
            listener.set_nonblocking(true).unwrap();
        }
        let local = format!("http://{}", local_listener.local_addr().unwrap());
        let public = format!("http://{}", public_listener.local_addr().unwrap());
        let dir = tempfile::tempdir().unwrap();
        let image_path = dir.path().join("probe.png");
        image::RgbImage::from_pixel(16, 16, image::Rgb([0x15, 0xba, 0x3f]))
            .save(&image_path)
            .unwrap();
        let file = url::Url::from_file_path(image_path).unwrap();
        let svg_https = public.replacen("http:", "https:", 1);
        let data_svg = base64::engine::general_purpose::STANDARD.encode(format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="16"><image href="{svg_https}/svg-data" width="16" height="16"/><image href="{file}" x="16" width="16" height="16"/></svg>"#
        ));

        // Deliberately bypass the sanitizer and CSP. The WebKit defenses
        // must still render the document without issuing a resource request.
        let html = format!(
            r#"<head>
            <link rel="preconnect" href="{public}">
            <link rel="dns-prefetch" href="//render-probe.invalid">
            <link rel="prefetch" href="{public}/prefetch">
            <link rel="icon" href="{local}/favicon">
            <meta http-equiv="refresh" content="0; url={local}/refresh">
            <style>@import "{public}/style";
            @im/*x*/port "{public}/split-style";
            @font-face {{ font-family: Probe; src: url({public}/font); }}
            p {{ font-family: Probe; background: image-set(url({public}/bg) 1x); }}
            div {{ background: url/*x*/({public}/split-bg); }}
            .image-set {{ background: image-set("{public}/quoted" 1x); }}
            .webkit {{ background: -webkit-image-set(url("") 1x, "{public}/webkit-quoted" 2x); }}
            .image {{ background: image("{public}/image"); }}
            .cross-fade {{ background: cross-fade("{public}/cross-fade", red); }}
            .var {{ --y: image-set("{public}/variable" 1x); background: var(--y); }}
            @media (width: 440px) {{ .width-probe {{ background-color: #7a163f; }} }}
            </style></head>
            <body><p class="width-probe">Offline preview</p><div>Probe</div>
            <script>document.querySelector('.width-probe').style.background = '#02e503';</script>
            <img src="data:image/svg+xml;base64,{data_svg}" width="32" height="16">
            <p class="image-set">Image set</p><p class="webkit">WebKit image set</p>
            <p class="image">Image</p><p class="cross-fade">Cross fade</p>
            <p class="var">Custom property</p>
            <p style='background: image-set("{public}/inline-set" 1x)'>Inline image set</p>
            <p style='background: -webkit-image-set(url("") 1x, "{public}/inline-webkit" 2x)'>Inline WebKit image set</p>
            <p style='background: image("{public}/inline-image")'>Inline image</p>
            <p style='background: cross-fade("{public}/inline-cross-fade", red)'>Inline cross fade</p>
            <p style='--y: image-set("{public}/inline-variable" 1x); background: var(--y)'>Inline custom property</p>
            <img src="{local}/pixel"><img src="{public}/pixel" srcset="{public}/srcset 2x">
            <img src="{file}" width="16" height="16">
            <iframe src="{public}/frame"></iframe>
            <form action="{local}/form"><input></form>
            <script>fetch("{local}/script");</script>
            <a href="{public}/link" ping="{local}/ping">Link</a>
            <svg xmlns="http://www.w3.org/2000/svg">
              <defs>
                <filter id="filter"><feImage href="{public}/fe-image" xlink:href="{local}/fe-xlink"/></filter>
                <linearGradient href="{public}/gradient"/>
                <pattern href="{file}"/>
                <filter href="{public}/filter"/>
                <cursor href="{public}/cursor"/>
                <font-face><font-face-src><font-face-uri href="{public}/font-face"/></font-face-src></font-face>
              </defs>
              <image href="{public}/svg" filter="url(#filter)" data-src="{local}/data">
                <set attributeName="href" to="{file}"/>
                <animate attributeName="href" values="{public}/values;relative.png"
                  from="{public}/from" to="{local}/to" by="{public}/by"/>
              </image>
              <text><textPath href="{public}/path">Text</textPath></text>
              <animateMotion><mpath href="{public}/motion"/></animateMotion>
              <rect width="10" height="10" fill="url({public}/fill)" stroke="url({file})"
                filter="url({public}/paint-filter)" mask="url({public}/mask)"
                clip-path="url({public}/clip)"/>
            </svg>
            </body>"#
        );
        let start = super::super::proxy::requests();
        let picture = futures::executor::block_on(render(WebRequest {
            html,
            width: 440,
            background: 0xffffff,
        }))
        .unwrap()
        .expect("WebKit drew the hostile document");
        assert!(
            contains_display_color(&picture, [0x7a, 0x16, 0x3f]),
            "the 440px CSS viewport paints #7a163f",
        );
        assert!(!contains_display_color(&picture, [0x3f, 0x16, 0x7a]));
        assert!(!contains_display_color(&picture, [0x02, 0xe5, 0x03]));
        assert!(!contains_display_color(&picture, [0x15, 0xba, 0x3f]));
        assert_eq!(super::super::proxy::requests() - start, 0);
        for listener in [local_listener, public_listener] {
            assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
        }
        eprintln!(
            "hostile WebKit corpus drew a frame without proxy, origin or file loads ({public})"
        );

        let blocked = fell_render::WebMail::prepare(
            r#"<style>@media (width:360px) { p { background:#7a163f; } }</style><p>Preview</p>
            <img src="https://cdn.example.test/width-only" width="300" alt="A photo" style="display:block">"#,
        );
        assert!(blocked.has_remote_images());
        let picture = estimate_covers_snapshot("blocked width-only image", &blocked, 360);
        assert!(
            contains_display_color(&picture, [0x7a, 0x16, 0x3f]),
            "the reused snapshot uses a 360px CSS viewport",
        );
        assert!(
            picture.height >= 190.,
            "placeholder height: {}",
            picture.height
        );

        let huge = fell_render::WebMail::prepare(
            r#"<p>River news</p><img src="https://cdn.example.test/huge"
            width="600" height="200000" style="display:block;min-height:200000px !important">"#,
        );
        let picture = estimate_covers_snapshot("oversized image with author minimum", &huge, 440);
        assert!(
            (680. ..740.).contains(&picture.height)
                && (0. ..=32.).contains(&(huge.estimated_height(440.) - picture.height)),
            "snapshot: {}, estimate: {}",
            picture.height,
            huge.estimated_height(440.),
        );

        let calibration = [
            ("inline-styled ESP table", 493, String::from(
            r#"<style>.masthead {display:flex;gap:12px}</style>
            <table role="presentation" width="100%" cellspacing="0" style="background:#f3f0e8;color:#263950">
            <tr><td style="padding:20px 22px 16px;font:14px sans-serif">
            <div class="masthead"><b>THE LAKESIDE DISPATCH</b><span>No. 42</span></div>
            <h1 style="margin:12px 0 8px;font:30px Georgia">The week the river slowed</h1>
            <p style="margin:0;font:16px Georgia">For three days the river barely moved. The ferry ran on time anyway.</p></td></tr>
            <tr><td style="padding:0 22px 20px">
            <img src="https://cdn.example.test/river" width="540" height="169" style="display:block">
            <table role="presentation" width="100%"><tr>
            <td style="padding-top:8px"><img src="https://cdn.example.test/pier" width="156" height="64"></td>
            <td style="padding-top:8px"><img src="https://cdn.example.test/library" width="156" height="64"></td>
            </tr></table></td></tr></table>"#,
            )),
            ("mobile reveal", 360, String::from(concat!(
                "<style>@media (max-width:600px) { .mobile { display:block !important; ",
                "visibility:visible !important; font-size:16px !important; ",
                "max-height:none !important; overflow:visible !important; background:#7a163f; } ",
                ".col{display:block!important;width:100%!important} } ",
                ".col{display:inline-block;width:48%;height:65px;background:#3f167a}</style>",
                r#"<div class="mobile" style="display:none;visibility:hidden;font-size:0;max-height:0;overflow:hidden">"#,
                "Mobile dispatch<br>Edition 42</div>",
                r#"<div class="col"></div><div class="col"></div>"#,
            ))),
            ("spacer cells and body padding", 440, String::from(concat!(
                r#"<style media="print">.show {display:none !important}</style>"#,
                r#"<style media="screen">.show {display:block !important;background:#7a163f}</style>"#,
                r#"<body style="padding-block:8px 12px"><div hidden class="show">Visible in web view</div>"#,
                r#"<table><tr height="32"><td height="24" style="font-size:0"></td></tr></table>"#,
                r#"<div style="min-height:86px;border-block-start:3px solid;border-block-end:5px solid;"#,
                r#"padding-block:4px 8px;margin-block:7px 9px;background:#3f167a">Spacer</div></body>"#,
            ))),
            ("hero and padded sections", 440, format!(
                r#"<style>
                  div.hero#lead {{height:280px;background:#7a163f}}
                  td.section {{padding:2em 20px;text-align:center}}
                  section.picture {{padding-top:50%;background:#eeede1}}
                </style>
                <div class="hero" id="lead">Along the river</div>
                <section class="picture">The park in autumn</section>
                <table width="100%" role="presentation" cellspacing="0">{}</table>"#,
                r#"<tr><td class="section" style="background:#eeede1">This week by the lake</td></tr>"#.repeat(4),
            )),
            ("column flex masthead", 440, String::from(concat!(
                r#"<div style="display:flex;flex-direction:column;gap:16px;row-gap:18px;padding:12px;background:#eeede1">"#,
                r#"<div style="height:75px">The Lakeside Dispatch</div>"#,
                r#"<div style="height:75px">Issue 42</div>"#,
                r#"<div style="height:110px;border-block:6px solid">The river in autumn</div></div>"#,
            ))),
            ("desktop and mobile pair", 440, String::from(concat!(
                r#"<style>.mobile {display:none} .desktop {height:320px;background:#3f167a}
                @media screen and (max-width:600px) {.desktop {display:none} .mobile {display:block;height:234px;background:#7a163f}}
                @media all and (min-width:601px) {.mobile {display:none}}</style>"#,
                r#"<div class="desktop">Desktop edition</div><div class="mobile">Mobile edition</div>"#,
            ))),
            ("hover and compact paragraphs", 440, format!(
                r#"<style>p {{margin:0}} a:hover {{display:none}} img {{border:0}}</style>{}"#,
                r#"<p style="text-align:left;font-family:serif;font-weight:normal"><a href="https://example.test">Around town</a> this week.</p>"#.repeat(8),
            )),
            ("retail", 600, include_str!("../../../fell-render/fixtures/corpus/esp-marketing.html").into()),
            ("ordinary newsletter", 680, include_str!("../../../fell-render/fixtures/web-newsletter.html").into()),
            ("white article", 440, include_str!("../../../fell-render/fixtures/web-white-article.html").into()),
            ("long article", 440, format!(
                r#"<article><h1 style="margin:0 0 12px">A walk along the lake</h1>{}</article>"#,
                "<p>On Saturday the first ferry left the harbor just after sunrise. The new footpath now follows the water from the boat landing to the old railway bridge.</p>".repeat(18),
            )),
            ("fixed-width article", 493, include_str!("../../../fell-render/fixtures/web-fixed-article.html").into()),
            ("SendGrid", 493, include_str!("../../../fell-render/fixtures/web-sendgrid.html").into()),
            ("ESP grouped reset", 530, String::from(concat!(
                "<style>body, table, td { margin:0; padding:0 } p, h1, td { margin:0 } ",
                ".card, .card:hover { height:400px } .card:hover { display:none }</style>",
                r#"<table width="600" cellspacing="0" style="background:#e7e2d8"><tr><td class="card">"#,
                "<h1>Northwind Notes</h1><p>The week the river slowed</p></td></tr></table>",
            ))),
            ("mobile reveal with padded columns", 432, String::from(concat!(
                "<style>.column { display:inline-block; width:47%; height:65px; background:#dcece5; ",
                "vertical-align:top; box-sizing:border-box; padding:8px } .mobile { display:none } ",
                "@media (max-width:600px) { .column { display:block; width:100% } ",
                ".mobile { display:block!important; max-height:none!important; overflow:visible!important; ",
                "font-size:16px!important; background:#f3e3ab; padding:8px } }</style>",
                r#"<article style="background:#f2ecdd;color:#243e43;padding:24px">"#,
                r#"<h1 style="font:30px Georgia;margin:0 0 12px">The short crossing</h1>"#,
                r#"<div class="mobile" style="display:none;max-height:0;overflow:hidden;font-size:0">"#,
                "On this screen: the next ferry leaves from the east pier.</div>",
                r#"<p style="margin:14px 0">The ferry runs on the hour from sunrise.</p>"#,
                r#"<div class="column">East pier<br>From 7:00</div>"#,
                r#"<div class="column">West pier<br>From 7:30</div></article>"#,
            ))),
        ];
        // Also check the production metrics on machines with the stock fonts.
        // The bundled font keeps the default CI measurements reproducible.
        let system_fonts = std::env::var_os("FELL_CALIBRATION_SYSTEM_FONTS").is_some();
        let mut outliers = Vec::new();
        for (label, width, html) in calibration {
            let mail = if system_fonts {
                fell_render::WebMail::prepare(&html)
            } else {
                fell_render::WebMail::calibration(&html)
            };
            let extra_widths: &[i32] = match label {
                "inline-styled ESP table" => &[560],
                "desktop and mobile pair" => &[680],
                "mobile reveal" => &[432, 490, 680],
                "mobile reveal with padded columns" => &[490, 680],
                "retail" => &[530, 680, 960],
                "fixed-width article" | "SendGrid" => &[435, 472, 530, 960],
                _ => &[],
            };
            for (scheme, background) in [("light", 0xffffff), ("dark", 0x24283b)] {
                let (_, background) = mail.appearance(scheme, background);
                for width in std::iter::once(width).chain(extra_widths.iter().copied()) {
                    let picture = futures::executor::block_on(render(WebRequest {
                        html: if system_fonts {
                            mail.page(scheme, |_| None)
                        } else {
                            mail.calibration_page(scheme)
                        },
                        width,
                        background,
                    }))
                    .unwrap()
                    .unwrap_or_else(|| panic!("WebKit painted {label}"));
                    let estimate = mail.estimated_height(width as f32);
                    let painted = picture.height * width as f32 / picture.width;
                    let delta = estimate - painted;
                    eprintln!(
                        "calibration | {label} | {scheme} | {width} | {estimate:.1} | {painted:.1} | {delta:+.1}"
                    );
                    if !(0. ..=25.).contains(&delta) {
                        outliers.push((label, scheme, width, delta));
                    }
                    if matches!(label, "mobile reveal" | "desktop and mobile pair") {
                        assert_eq!(
                            contains_display_color(&picture, [0x7a, 0x16, 0x3f]),
                            width < 600
                        );
                        assert_eq!(
                            contains_display_color(&picture, [0x3f, 0x16, 0x7a]),
                            label == "mobile reveal" || width > 600
                        );
                    }
                }
            }
        }
        assert!(
            outliers.is_empty(),
            "outside the 0–25px allowance: {outliers:?}"
        );

        let dark_mail = |html| {
            let mail = fell_render::WebMail::prepare(html);
            let (scheme, background) = mail.appearance("dark", 0x24283b);
            let picture = futures::executor::block_on(render(WebRequest {
                html: mail.page_with_font(scheme, Some("Inter"), |_| None),
                width: 440,
                background,
            }))
            .unwrap()
            .expect("WebKit painted mail in dark mode");
            assert!(mail.estimated_height(440.) >= picture.height * 440. / picture.width);
            picture
        };
        let paper = dark_mail(concat!(
            r##"<table width="600" bgcolor="#ffffff"><tr><td style="font:bold 24px sans-serif">"##,
            r#"<div>News from the harbor</div><div style="color:#555">The ferry is open</div>"#,
            r#"<div style="background:#e7e2d8"><a href="https://example.test">Timetable</a></div></td></tr></table>"#,
        ));
        for (text, background) in [
            ([0, 0, 0], [255; 3]),
            ([0x55; 3], [255; 3]),
            ([0, 0, 0xee], [0xe7, 0xe2, 0xd8]),
        ] {
            assert!(
                contains_display_color(&paper, text),
                "missing text color {text:?}"
            );
            assert!(contains_display_color(&paper, background));
            let rgb = |[r, g, b]: [u8; 3]| u32::from_be_bytes([0, r, g, b]);
            assert!(crate::theme::contrast(rgb(text), rgb(background)) >= 4.5);
        }
        let personal = dark_mail("<div>Booked. The hall opens at noon.</div>");
        assert!(contains_display_color(&personal, [0x24, 0x28, 0x3b]));
        let pixels = personal.image.as_bytes(0).unwrap().as_chunks::<4>().0;
        let width = personal.width as usize;
        let surface = [0x3b, 0x28, 0x24, 255];
        assert!(pixels[..12 * width].iter().all(|pixel| *pixel == surface));
        assert!(
            pixels
                .chunks_exact(width)
                .all(|row| row[..14].iter().all(|pixel| *pixel == surface))
        );

        let hero = fell_render::WebMail::prepare(
            r#"<div style="height:1000px;background:#7a163f">A tall hero</div>"#,
        );
        let picture = estimate_covers_snapshot("solid hero", &hero, 440);
        assert_eq!(picture.height, 1000.);
        assert!(!picture.clipped);

        let tall = fell_render::WebMail::prepare(
            r#"<div style="height:200000px"><span style="background:#7a163f">Tall page</span></div>
               <div style="background:#3f167a">Offscreen</div>"#,
        );
        let picture = estimate_covers_snapshot("clipped page with a uniform tail", &tall, 440);
        assert!(picture.clipped);
        assert!(picture.document_height >= 200_000.);
        assert_eq!(picture.last_row_color, 0xffffff);
        assert_eq!(picture.height, SNAPSHOT_HEIGHT_LIMIT as f32);
        assert!(contains_display_color(&picture, [0x7a, 0x16, 0x3f]));
        assert!(!contains_display_color(&picture, [0x3f, 0x16, 0x7a]));

        let mut png = std::io::Cursor::new(Vec::new());
        let image = image::RgbImage::from_fn(1024, 768, |x, _| {
            image::Rgb(if x < 512 {
                [0xd1, 0x63, 0x26]
            } else {
                [0x25, 0x79, 0x9d]
            })
        });
        image.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let png = base64::engine::general_purpose::STANDARD.encode(png.get_ref());
        let images = fell_render::WebMail::prepare(&format!(
            r#"<p style="background:#7a163f">Local image below</p>
            <img src="data:image/png;base64,AAAA" width="160" height="26" alt="Damaged image" style="display:block">
            <img loading="lazy" src="data:image/png;base64,{png}" width="320" height="240">"#,
        ));
        let picture = estimate_covers_snapshot("broken image beside a decoded image", &images, 360);
        assert!(contains_display_color(&picture, [0x7a, 0x16, 0x3f]));
        assert!(contains_display_color(&picture, [0xd1, 0x63, 0x26]));
        assert!(contains_display_color(&picture, [0x25, 0x79, 0x9d]));

        let padded = |padding| {
            let mail = fell_render::WebMail::prepare(&format!(
                r#"<body style="padding-bottom:{padding}px"><div style="height:96px">Padding probe</div></body>"#,
            ));
            estimate_covers_snapshot("body padding", &mail, 360).height
        };
        let default = padded(0);
        assert_eq!(padded(24) - default, 24.);
        assert_eq!(padded(400) - default, 400.);

        let blank = futures::executor::block_on(render(WebRequest {
            html: "<!doctype html><style>body { height: 240px; }</style>".into(),
            width: 360,
            background: 0xffffff,
        }));
        assert!(
            blank.unwrap().is_none(),
            "a uniform capture is a failed snapshot"
        );
    }
}
