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
use webkit2gtk::{PolicyDecisionExt, SettingsExt, WebViewExt};
use wry::{WebViewBuilderExtUnix, WebViewExtUnix};

#[derive(Clone)]
pub(crate) struct Snapshot {
    pub image: Arc<RenderImage>,
    pub width: f32,
    pub height: f32,
}

pub(crate) struct WebRequest {
    pub html: String,
    pub width: i32,
    pub background: u32,
    pub settle_images: bool,
}

struct Job {
    request: WebRequest,
    reply: oneshot::Sender<Snapshot>,
}

#[derive(Default)]
struct JobQueue(Mutex<Option<Job>>);

impl JobQueue {
    fn submit(&self, job: Job) {
        let superseded = self.0.lock().unwrap().replace(job);
        drop(superseded);
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
    reply: oneshot::Sender<Snapshot>,
    cancel: gtk::gio::Cancellable,
    settle_images: bool,
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

pub(crate) async fn render(request: WebRequest) -> Option<Snapshot> {
    let (reply, done) = oneshot::channel();
    let worker = worker()?;
    worker.queue.submit(Job { request, reply });
    worker.wake.send(()).ok()?;
    // The worker can fail during setup, before GLib attaches its receiver.
    // Bound the wait even when GLib keeps an already queued request alive.
    let timeout = async_io::Timer::after(Duration::from_secs(6));
    match futures::future::select(done, timeout).await {
        futures::future::Either::Left((result, _)) => result.ok(),
        futures::future::Either::Right(_) => None,
    }
}

#[allow(deprecated)]
fn run(rx: glib::Receiver<()>, queue: Arc<JobQueue>) -> Result<(), Box<dyn std::error::Error>> {
    gtk::init()?;
    let window = gtk::OffscreenWindow::new();
    window.set_accept_focus(false);
    window.set_default_size(640, 240);
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    window.add(&container);
    let port = super::proxy::port().ok_or("could not start the WebKit deny proxy")?;
    let webview = wry::WebViewBuilder::new()
        .with_proxy_config(wry::ProxyConfig::Http(wry::ProxyEndpoint {
            host: "127.0.0.1".into(),
            port: port.to_string(),
        }))
        .with_incognito(true)
        .with_javascript_disabled()
        // Leave Wry's opt-in Allow policy off. secure() supplies Deny for each load.
        .with_autoplay(false)
        .with_clipboard(false)
        .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
        .build_gtk(&container)?;
    let widget = webview.webview();
    secure(&widget)?;
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
        move |_, _| cancel(&active)
    });
    window.show_all();
    // Start the web process before the first conversation needs it.
    widget.load_html("<!doctype html>", None);
    let _source = rx.attach(None, move |()| {
        let Some(job) = queue.next() else {
            return glib::ControlFlow::Continue;
        };
        cancel(&active);
        widget.stop_loading();
        let WebRequest {
            html,
            width,
            background,
            settle_images,
        } = job.request;
        window.resize(width, 240);
        let channel = |shift| f64::from((background >> shift) & 255_u32) / 255.0;
        widget.set_background_color(&gtk::gdk::RGBA::new(
            channel(16),
            channel(8),
            channel(0),
            1.0,
        ));
        let cancellable = gtk::gio::Cancellable::new();
        *active.borrow_mut() = Some(Load {
            reply: job.reply,
            cancel: cancellable.clone(),
            settle_images,
        });
        widget.load_html(&html, None);
        glib::timeout_add_local_once(Duration::from_secs(4), {
            let active = active.clone();
            let widget = widget.clone();
            move || {
                if active
                    .borrow()
                    .as_ref()
                    .is_some_and(|load| load.cancel == cancellable)
                {
                    cancel(&active);
                    widget.stop_loading();
                }
            }
        });
        glib::ControlFlow::Continue
    });
    gtk::main();
    Ok(())
}

fn cancel(active: &Active) {
    let old = active.borrow_mut().take();
    if let Some(load) = old {
        load.cancel.cancel();
    }
}

fn schedule_capture(widget: &webkit2gtk::WebView, active: &Active) {
    let Some((token, settle_images)) = active
        .borrow()
        .as_ref()
        .map(|load| (load.cancel.clone(), load.settle_images))
    else {
        return;
    };
    if settle_images {
        // WebKit emits Finished before a decoded image's next paint. Give
        // local image decoders a refresh interval before asking for pixels.
        let widget = widget.clone();
        let active = active.clone();
        glib::timeout_add_local_once(Duration::from_millis(80), move || {
            capture(&widget, active, token);
        });
    } else {
        capture(widget, active.clone(), token);
    }
}

fn capture(widget: &webkit2gtk::WebView, active: Active, token: gtk::gio::Cancellable) {
    if token.is_cancelled() {
        return;
    }
    widget.snapshot(
        webkit2gtk::SnapshotRegion::FullDocument,
        webkit2gtk::SnapshotOptions::NONE,
        Some(&token.clone()),
        move |result| {
            if token.is_cancelled() {
                return;
            }
            if let Some(load) = active.borrow_mut().take()
                && let Some(image) = result.ok().and_then(surface_image)
            {
                let _ = load.reply.send(image);
            }
        },
    );
}

fn secure(widget: &webkit2gtk::WebView) -> Result<(), Box<dyn std::error::Error>> {
    let settings = WebViewExt::settings(widget).ok_or("missing WebKit settings")?;
    settings.set_enable_javascript(false);
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

fn surface_image(surface: gtk::cairo::Surface) -> Option<Snapshot> {
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
    // Cairo pixels are premultiplied. GPUI takes unpremultiplied BGRA.
    for [b, g, r, a] in bgra.as_chunks_mut::<4>().0 {
        if *a != 0 && *a != 255 {
            let alpha = u16::from(*a);
            *b = ((u16::from(*b) * 255) / alpha) as u8;
            *g = ((u16::from(*g) * 255) / alpha) as u8;
            *r = ((u16::from(*r) * 255) / alpha) as u8;
        }
    }
    let dst_h = super::native::trim_uniform_bottom(&bgra, dst_w, dst_h).clamp(1, dst_h);
    bgra.truncate(dst_w as usize * dst_h as usize * 4);
    let pixels = image::RgbaImage::from_raw(dst_w, dst_h, bgra)?;
    Some(Snapshot {
        image: Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(
            pixels
        )])),
        width: dst_w as f32,
        height: dst_h as f32,
    })
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::net::{TcpListener, UdpSocket};

    use super::*;

    #[test]
    fn only_the_latest_queued_snapshot_is_loaded() {
        let queue = JobQueue::default();
        let replies = ["first", "second", "third"].map(|html| {
            let (reply, done) = oneshot::channel();
            queue.submit(Job {
                request: WebRequest {
                    html: html.into(),
                    width: 440,
                    background: 0xffffff,
                    settle_images: false,
                },
                reply,
            });
            done
        });
        assert_eq!(queue.next().unwrap().request.html, "third");
        assert!(queue.next().is_none());
        drop(replies);
    }

    #[test]
    fn hostile_email_makes_zero_webkit_requests() {
        use fell_core::test_support::skip_unless_available;

        const TEST: &str = "render::webview::tests::hostile_email_makes_zero_webkit_requests";
        let no_display =
            std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none();
        if no_display
            && (std::env::var_os("CI").is_some_and(|ci| ci == "true")
                || std::env::var_os("FELL_REQUIRE_ALL_TESTS").is_some_and(|set| set == "1"))
        {
            // The Ubuntu runner ships Xvfb. Give just this test a display,
            // leaving the rest of the suite independent of GTK.
            let output = std::process::Command::new("xvfb-run")
                .args(["-a", "--"])
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env("GDK_BACKEND", "x11")
                .env_remove("DBUS_SESSION_BUS_ADDRESS")
                .output()
                .expect("start WebKit test on the CI virtual display");
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        if skip_unless_available(
            TEST,
            no_display.then(|| "WebKit requires an isolated Wayland or X11 display".into()),
        ) {
            return;
        }

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
            </style></head>
            <body><p style="background-color:#7a163f">Offline preview</p><div>Probe</div>
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
            settle_images: true,
        }))
        .expect("WebKit drew the hostile document");
        let pixels = picture.image.as_bytes(0).unwrap().as_chunks::<4>().0;
        assert!(pixels.contains(&[0x3f, 0x16, 0x7a, 255]));
        assert!(!pixels.contains(&[0x3f, 0xba, 0x15, 255]));
        assert_eq!(super::super::proxy::requests() - start, 0);
        for listener in [local_listener, public_listener] {
            assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
        }
        eprintln!("hostile WebKit corpus drew a frame without proxy, origin or file loads ({public})");

        let blocked = fell_render::WebMail::prepare(
            r#"<img src="https://cdn.example.test/width-only" width="300" alt="A photo" style="display:block">"#,
        );
        assert!(blocked.has_remote_images());
        let picture = futures::executor::block_on(render(WebRequest {
            html: blocked.page("light", |_| None),
            width: 440,
            background: 0xffffff,
            settle_images: true,
        }))
        .expect("WebKit drew the blocked image placeholder");
        assert!(
            picture.height >= 190.,
            "placeholder height: {}",
            picture.height
        );
    }
}
