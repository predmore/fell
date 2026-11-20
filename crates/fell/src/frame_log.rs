//! `FELL_FRAME_LOG=<path>`: one JSON line per inbox frame, for checking the
//! 8 ms key-to-frame budget while animations run.
//!
//! Each line has `t_ms` (when the frame started, since the log opened),
//! `frame_ms` (render, layout, prepaint and paint on the UI
//! thread), `build_ms` (the part of it spent building the element tree),
//! `key_ms` on the first frame after a key (key handler start to the end of
//! that frame's paint), `cpu_ms` (CPU time the UI thread spent on the frame,
//! which a busy machine's preemption does not inflate), the key, and
//! whether a transition was running. GPU
//! submission and presentation happen after paint and are not counted in
//! the frame; `gap_ms` and `between_cpu_ms` (wall and UI-thread CPU time
//! from the last frame's paint to this frame's start) show them, and any
//! other UI-thread work between frames.
//!
//! Work the UI thread does outside a frame (a store reply landing) is a
//! line of its own: `event`, its `ms` and `cpu_ms`.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::rc::Rc;
use std::time::Instant;

pub struct FrameLog {
    out: BufWriter<File>,
    opened: Instant,
    render_at: Option<Instant>,
    render_cpu: f64,
    built_at: Option<Instant>,
    /// Phase marks: layout done (first prepaint), prepaint done, paint started.
    marks: [Option<Instant>; 3],
    key: Option<(Instant, String)>,
    animating: bool,
    /// When the last frame's paint ended, and the thread's CPU time then.
    last_paint: Option<(Instant, f64)>,
    gap: Option<(f64, f64)>,
}

pub type SharedFrameLog = Rc<RefCell<FrameLog>>;

fn ms(from: Instant, to: Instant) -> f64 {
    to.duration_since(from).as_secs_f64() * 1000.0
}

/// CPU time this thread has used, in milliseconds.
fn thread_cpu_ms() -> f64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` is a valid timespec for the call to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
    time.tv_sec as f64 * 1000.0 + time.tv_nsec as f64 / 1_000_000.0
}

impl FrameLog {
    pub fn from_env() -> Option<SharedFrameLog> {
        let path = std::env::var_os("FELL_FRAME_LOG")?;
        // Key names and timings are the reader's: only they may read them.
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .ok()?;
        Some(Rc::new(RefCell::new(FrameLog {
            out: BufWriter::new(file),
            opened: Instant::now(),
            render_at: None,
            render_cpu: 0.0,
            built_at: None,
            marks: [None; 3],
            key: None,
            animating: false,
            last_paint: None,
            gap: None,
        })))
    }

    pub fn key(&mut self, at: Instant, key: String) {
        // A second key before a frame: measure from the first.
        if self.key.is_none() {
            self.key = Some((at, key));
        }
    }

    pub fn render_started(&mut self, at: Instant, animating: bool) {
        self.render_at = Some(at);
        self.render_cpu = thread_cpu_ms();
        self.animating = animating;
        self.gap = self
            .last_paint
            .map(|(painted, cpu)| (ms(painted, at), self.render_cpu - cpu));
    }

    /// Time a piece of UI-thread work outside a frame, as an `event` line.
    pub fn event<T>(log: Option<&SharedFrameLog>, name: &str, work: impl FnOnce() -> T) -> T {
        let Some(log) = log else {
            return work();
        };
        let (start, cpu) = (Instant::now(), thread_cpu_ms());
        let out = work();
        let (end, cpu_ms) = (Instant::now(), thread_cpu_ms() - cpu);
        let mut log = log.borrow_mut();
        let t_ms = ms(log.opened, start);
        let _ = writeln!(
            log.out,
            "{{\"t_ms\":{t_ms:.1},\"event\":{name:?},\"ms\":{:.3},\"cpu_ms\":{cpu_ms:.3}}}",
            ms(start, end)
        );
        let _ = log.out.flush();
        out
    }

    /// A value the frame being drawn decided, as a `note` line: the
    /// conversation offset the landing hold set before paint.
    pub fn note(&mut self, name: &str, value: f32) {
        let t_ms = ms(self.opened, Instant::now());
        let _ = writeln!(
            self.out,
            "{{\"t_ms\":{t_ms:.1},\"note\":{name:?},\"value\":{value:.1}}}"
        );
        let _ = self.out.flush();
    }

    /// The store thread's query interval, expressed on the frame clock.
    pub fn search_work(&mut self, start: Instant, end: Instant) {
        let t_ms = ms(self.opened, start);
        let duration = ms(start, end);
        let _ = writeln!(
            self.out,
            "{{\"t_ms\":{t_ms:.3},\"event\":\"search_work\",\"ms\":{duration:.3}}}"
        );
        let _ = self.out.flush();
    }

    /// The view's element tree is built; layout and paint follow.
    pub fn built(&mut self, at: Instant) {
        self.built_at = Some(at);
    }

    /// The first element's prepaint (layout is done), the last element's
    /// prepaint (prepaint is done), and the first element's paint.
    pub fn mark(&mut self, phase: usize, at: Instant) {
        if let Some(slot) = self.marks.get_mut(phase) {
            *slot = Some(at);
        }
    }

    pub fn painted(&mut self, at: Instant) {
        let Some(start) = self.render_at.take() else {
            return;
        };
        let frame_ms = ms(start, at);
        let cpu_ms = thread_cpu_ms() - self.render_cpu;
        let built = self.built_at.take();
        let build_ms = built.map_or(0.0, |built| ms(start, built));
        let [laid, prepainted, painting] = std::mem::take(&mut self.marks);
        let span = |from: Option<Instant>, to: Option<Instant>| match (from, to) {
            (Some(from), Some(to)) => format!("{:.3}", ms(from, to)),
            _ => "null".to_string(),
        };
        let layout_ms = span(built, laid);
        let prepaint_ms = span(laid, prepainted);
        let paint_ms = span(painting, Some(at));
        let (key_ms, name) = match self.key.take() {
            Some((pressed, name)) => (format!("{:.3}", ms(pressed, at)), name),
            None => ("null".to_string(), String::new()),
        };
        let (gap_ms, between_cpu_ms) = match self.gap.take() {
            Some((gap, cpu)) => (format!("{gap:.3}"), format!("{cpu:.3}")),
            None => ("null".to_string(), "null".to_string()),
        };
        let _ = writeln!(
            self.out,
            "{{\"t_ms\":{:.1},\"frame_ms\":{frame_ms:.3},\"build_ms\":{build_ms:.3},\"layout_ms\":{layout_ms},\"prepaint_ms\":{prepaint_ms},\"paint_ms\":{paint_ms},\"cpu_ms\":{cpu_ms:.3},\"gap_ms\":{gap_ms},\"between_cpu_ms\":{between_cpu_ms},\"key_ms\":{key_ms},\"key\":{name:?},\"anim\":{}}}",
            ms(self.opened, start),
            self.animating
        );
        self.last_paint = Some((at, thread_cpu_ms()));
        let _ = self.out.flush();
    }
}
