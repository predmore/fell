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
//! submission and presentation happen after paint and are not counted.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::rc::Rc;
use std::time::Instant;

pub struct FrameLog {
    out: BufWriter<File>,
    opened: Instant,
    render_at: Option<Instant>,
    render_cpu: f64,
    built_at: Option<Instant>,
    key: Option<(Instant, String)>,
    animating: bool,
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
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        Some(Rc::new(RefCell::new(FrameLog {
            out: BufWriter::new(file),
            opened: Instant::now(),
            render_at: None,
            render_cpu: 0.0,
            built_at: None,
            key: None,
            animating: false,
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
    }

    /// The view's element tree is built; layout and paint follow.
    pub fn built(&mut self, at: Instant) {
        self.built_at = Some(at);
    }

    pub fn painted(&mut self, at: Instant) {
        let Some(start) = self.render_at.take() else {
            return;
        };
        let frame_ms = ms(start, at);
        let cpu_ms = thread_cpu_ms() - self.render_cpu;
        let build_ms = self.built_at.take().map_or(0.0, |built| ms(start, built));
        let (key_ms, name) = match self.key.take() {
            Some((pressed, name)) => (format!("{:.3}", ms(pressed, at)), name),
            None => ("null".to_string(), String::new()),
        };
        let _ = writeln!(
            self.out,
            "{{\"t_ms\":{:.1},\"frame_ms\":{frame_ms:.3},\"build_ms\":{build_ms:.3},\"cpu_ms\":{cpu_ms:.3},\"key_ms\":{key_ms},\"key\":{name:?},\"anim\":{}}}",
            ms(self.opened, start),
            self.animating
        );
        let _ = self.out.flush();
    }
}
