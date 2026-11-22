//! Opt-in inbox timings. CPU paint and compositor presentation are separate.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs::{DirBuilder, File};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use gpui::{Global, Window};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::protocol::{wl_callback, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::presentation_time::client::{wp_presentation, wp_presentation_feedback};

#[derive(Clone)]
pub(crate) struct UiTrace(Rc<RefCell<Trace>>);

impl Global for UiTrace {}

struct Trace {
    out: BufWriter<File>,
    start: Instant,
    keys: Vec<Key>,
    awaiting_presentation: VecDeque<Key>,
    next_key: u64,
    first_paint: bool,
    ready_paint: bool,
    folder_ready: bool,
    first_presented: bool,
    ready_presented: bool,
    presentation: Option<Presentation>,
    polling: bool,
    painted_ms: Vec<f64>,
    presented_ms: Vec<f64>,
    finished: bool,
}

struct Key {
    id: u64,
    at: Instant,
    action: &'static str,
}

struct Frame {
    keys_through: Option<u64>,
    usable: bool,
}

impl UiTrace {
    pub(crate) fn open(start: Instant) -> io::Result<Self> {
        let state_home = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no state directory"))?;
        let dir = state_home.join("fell/trace");
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let (file, path) = tempfile::Builder::new()
            .prefix("inbox-")
            .suffix(".jsonl")
            .tempfile_in(dir)?
            .keep()?;
        let _ = writeln!(io::stderr(), "fell: UI trace at {}", path.display());
        let mut trace = Trace {
            out: BufWriter::new(file),
            start,
            keys: Vec::new(),
            awaiting_presentation: VecDeque::new(),
            next_key: 0,
            first_paint: false,
            ready_paint: false,
            folder_ready: false,
            first_presented: false,
            ready_presented: false,
            presentation: None,
            polling: false,
            painted_ms: Vec::new(),
            presented_ms: Vec::new(),
            finished: false,
        };
        trace.event(format_args!(r#"{{"event":"process_start","at_ms":0}}"#));
        Ok(Self(Rc::new(RefCell::new(trace))))
    }

    pub(crate) fn window_created(&self, window: &Window) {
        let mut trace = self.0.borrow_mut();
        trace.milestone("window_created");
        trace.presentation = Presentation::connect(window);
        if trace.presentation.is_none() {
            trace.milestone("presentation_unavailable");
        }
        drop(trace);
        self.schedule_poll(window);
    }

    pub(crate) fn key(&self, at: Instant, action: &'static str) {
        let mut trace = self.0.borrow_mut();
        let id = trace.next_key;
        trace.next_key += 1;
        let at_ms = at.duration_since(trace.start).as_secs_f64() * 1000.;
        trace.event(format_args!(
            r#"{{"event":"key_received","key_id":{id},"action":{action:?},"at_ms":{at_ms:.3}}}"#
        ));
        trace.keys.push(Key { id, at, action });
    }

    pub(crate) fn folder_ready(&self) {
        self.0.borrow_mut().folder_ready = true;
    }

    pub(crate) fn store_opened(&self, error: Option<&str>) {
        let event = if error.is_some() {
            fell_core::diagnostic!(WARN, "UI trace recorded a failed store open");
            "store_open_failed"
        } else {
            "store_opened"
        };
        self.0.borrow_mut().milestone(event);
    }

    pub(crate) fn painted(&self, window: &Window) {
        let mut trace = self.0.borrow_mut();
        let painted = trace.start.elapsed().as_secs_f64() * 1000.;
        if !trace.first_paint {
            trace.first_paint = true;
            trace.milestone("first_paint");
        }
        let usable = trace.folder_ready && !trace.ready_paint;
        if usable {
            trace.ready_paint = true;
            trace.milestone("usable_inbox_paint");
        }
        let keys = std::mem::take(&mut trace.keys);
        for key in &keys {
            let elapsed = trace.latency("key_painted", key, painted);
            trace.painted_ms.push(elapsed);
        }
        let usable = trace.folder_ready && !trace.ready_presented;
        if trace.presentation.is_some() {
            trace.awaiting_presentation.extend(keys);
            let keys_through = trace.awaiting_presentation.back().map(|key| key.id);
            if (keys_through.is_some() || !trace.first_presented || usable)
                && let Some(probe) = trace.presentation.as_mut()
            {
                probe.request(
                    window,
                    Frame {
                        keys_through,
                        usable,
                    },
                );
            }
        }
        drop(trace);
        self.schedule_poll(window);
    }

    pub(crate) fn finish(&self) {
        let mut trace = self.0.borrow_mut();
        if trace.finished {
            return;
        }
        trace.poll();
        trace.presentation = None;
        trace.finished = true;
        let trace = &mut *trace;
        let received = trace.next_key;
        for (label, samples) in [
            ("CPU paint", &mut trace.painted_ms),
            ("compositor presentation", &mut trace.presented_ms),
        ] {
            samples.sort_by(f64::total_cmp);
            if let Some(max) = samples.last() {
                let p50 = samples[samples.len().div_ceil(2) - 1];
                let p95 = samples[(samples.len() * 95).div_ceil(100) - 1];
                let _ = writeln!(
                    io::stderr(),
                    "fell: {label}: {}/{received} keys, p50 {p50:.3} ms, p95 {p95:.3} ms, max {max:.3} ms",
                    samples.len(),
                );
            } else {
                let _ = writeln!(io::stderr(), "fell: {label}: 0/{received} keys timed");
            }
        }
    }

    fn schedule_poll(&self, window: &Window) {
        let mut trace = self.0.borrow_mut();
        let waiting = trace
            .presentation
            .as_ref()
            .is_some_and(Presentation::waiting);
        if !waiting || trace.polling {
            return;
        }
        trace.polling = true;
        let this = self.clone();
        window.on_next_frame(move |window, _| {
            let mut trace = this.0.borrow_mut();
            trace.poll();
            if let Some(probe) = &trace.presentation
                && probe.state.registry_done
                && probe.state.pending == 0
                && (((!trace.first_presented || (trace.folder_ready && !trace.ready_presented))
                    && trace.start.elapsed().as_secs() < 5)
                    || trace
                        .awaiting_presentation
                        .back()
                        .is_some_and(|key| key.at.elapsed().as_secs_f32() < 1.))
            {
                // A discarded commit can leave a painted scene without any
                // feedback. Ask the next frame to submit it again.
                window.refresh();
            }
            drop(trace);
            this.schedule_poll(window);
        });
    }
}

impl Trace {
    fn event(&mut self, event: std::fmt::Arguments<'_>) {
        let _ = writeln!(self.out, "{event}");
        let _ = self.out.flush();
    }

    fn milestone(&mut self, name: &str) {
        let at = self.start.elapsed().as_secs_f64() * 1000.;
        self.event(format_args!(r#"{{"event":"{name}","at_ms":{at:.3}}}"#));
    }

    fn latency(&mut self, event: &str, key: &Key, frame_ms: f64) -> f64 {
        let at = key.at.duration_since(self.start).as_secs_f64() * 1000.;
        let latency = frame_ms - at;
        let id = key.id;
        let action = key.action;
        self.event(format_args!(
            r#"{{"event":"{event}","key_id":{id},"action":{action:?},"at_ms":{at:.3},"latency_ms":{latency:.3}}}"#
        ));
        latency
    }

    fn poll(&mut self) {
        self.polling = false;
        let Some(probe) = &mut self.presentation else {
            return;
        };
        let result = probe.queue.dispatch_pending(&mut probe.state);
        let feedback = std::mem::take(&mut probe.state.completed);
        let clock = match probe.state.clock {
            Some(clock) if !feedback.is_empty() => ClockSample::read(clock, self.start),
            _ => None,
        };
        if result.is_err()
            || (probe.state.registry_done && probe.state.global.is_none())
            || probe.state.unsupported_clock
        {
            self.presentation = None;
            self.awaiting_presentation.clear();
            self.milestone("presentation_unavailable");
        }
        for (frame, time) in feedback {
            if let Some(at) = time.and_then(|time| clock.as_ref()?.elapsed_ms(time)) {
                if !self.first_presented {
                    self.first_presented = true;
                    self.event(format_args!(
                        r#"{{"event":"first_frame_presented","at_ms":{at:.3}}}"#
                    ));
                }
                if frame.usable && !self.ready_presented {
                    self.ready_presented = true;
                    self.event(format_args!(
                        r#"{{"event":"usable_inbox_presented","at_ms":{at:.3}}}"#
                    ));
                }
                // A newer displayed scene covers keys even if the compositor
                // discarded an intermediate commit.
                while self.awaiting_presentation.front().is_some_and(|key| {
                    frame.keys_through.is_some_and(|last| key.id <= last)
                        && at >= key.at.duration_since(self.start).as_secs_f64() * 1000.
                }) {
                    let key = self.awaiting_presentation.pop_front().unwrap();
                    let elapsed = self.latency("key_presented", &key, at);
                    self.presented_ms.push(elapsed);
                }
            } else {
                self.milestone("presentation_commit_discarded");
            }
        }
    }
}

struct ClockSample {
    elapsed_ms: f64,
    compositor_ns: u64,
}

impl ClockSample {
    fn read(clock: libc::clockid_t, start: Instant) -> Option<Self> {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let before = start.elapsed();
        // SAFETY: clock_gettime writes a timespec to the supplied pointer.
        if unsafe { libc::clock_gettime(clock, &mut time) } != 0 {
            return None;
        }
        let elapsed = before + (start.elapsed() - before) / 2;
        Some(Self {
            elapsed_ms: elapsed.as_secs_f64() * 1000.,
            compositor_ns: seconds_and_nanos(
                time.tv_sec.try_into().ok()?,
                time.tv_nsec.try_into().ok()?,
            )?,
        })
    }

    fn elapsed_ms(&self, presented: u64) -> Option<f64> {
        let age = self.compositor_ns.checked_sub(presented)? as f64 / 1_000_000.;
        let elapsed = self.elapsed_ms - age;
        (elapsed >= 0.).then_some(elapsed)
    }
}

fn seconds_and_nanos(seconds: u64, nanos: u32) -> Option<u64> {
    if nanos >= 1_000_000_000 {
        return None;
    }
    seconds
        .checked_mul(1_000_000_000)?
        .checked_add(u64::from(nanos))
}

struct Presentation {
    connection: Connection,
    queue: EventQueue<PresentationState>,
    state: PresentationState,
}

#[derive(Default)]
struct PresentationState {
    global: Option<wp_presentation::WpPresentation>,
    registry_done: bool,
    clock: Option<libc::clockid_t>,
    unsupported_clock: bool,
    pending: usize,
    completed: Vec<(Frame, Option<u64>)>,
}

impl Presentation {
    fn connect(window: &Window) -> Option<Self> {
        let RawDisplayHandle::Wayland(display) = window.display_handle().ok()?.as_raw() else {
            return None;
        };
        // SAFETY: GPUI owns the display connection for the lifetime of this
        // window. The inbox close and app quit callbacks release the guest
        // backend and its queue while GPUI still owns the connection.
        let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        connection.display().sync(&queue.handle(), ());
        Some(Self {
            connection,
            queue,
            state: PresentationState::default(),
        })
    }

    fn waiting(&self) -> bool {
        !self.state.registry_done || self.state.pending > 0
    }

    fn request(&mut self, window: &Window, frame: Frame) -> Option<()> {
        let protocol = self.state.global.as_ref()?;
        let RawWindowHandle::Wayland(handle) =
            HasWindowHandle::window_handle(window).ok()?.as_raw()
        else {
            return None;
        };
        // SAFETY: the handle borrows this live window's wl_surface, and the
        // feedback request uses it only here. GPUI still owns the surface.
        let id = unsafe {
            ObjectId::from_ptr(
                wl_surface::WlSurface::interface(),
                handle.surface.as_ptr().cast(),
            )
        }
        .ok()?;
        let surface = wl_surface::WlSurface::from_id(&self.connection, id).ok()?;
        protocol.feedback(
            &surface,
            &self.queue.handle(),
            std::sync::Mutex::new(Some(frame)),
        );
        self.state.pending += 1;
        Some(())
    }
}

impl Drop for Presentation {
    fn drop(&mut self) {
        if let Some(global) = self.state.global.take() {
            global.destroy();
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for PresentationState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
            && interface == wp_presentation::WpPresentation::interface().name
        {
            state.global = Some(registry.bind(name, 1, queue, ()));
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for PresentationState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.registry_done = true;
    }
}

impl Dispatch<wp_presentation::WpPresentation, ()> for PresentationState {
    fn event(
        state: &mut Self,
        _: &wp_presentation::WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            match clk_id as libc::clockid_t {
                id @ (libc::CLOCK_MONOTONIC | libc::CLOCK_MONOTONIC_RAW | libc::CLOCK_BOOTTIME) => {
                    state.clock = Some(id);
                }
                _ => state.unsupported_clock = true,
            }
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, std::sync::Mutex<Option<Frame>>>
    for PresentationState
{
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        data: &std::sync::Mutex<Option<Frame>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wp_presentation_feedback::Event;
        let time = match event {
            Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                ..
            } => seconds_and_nanos((u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo), tv_nsec),
            Event::Discarded => None,
            _ => return,
        };
        if let Some(frame) = data.lock().unwrap().take() {
            state.pending -= 1;
            state.completed.push((frame, time));
        }
    }
}
