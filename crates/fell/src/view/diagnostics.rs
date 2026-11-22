use std::env;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use fell_core::diagnostics::{DiagnosticFile, take_crash_notice};
use futures::StreamExt as _;
use gpui::Context;

use super::Inbox;
use crate::keymap::{self, Action};
use crate::model::Toast;
use crate::worker::WorkerEvent;

impl Inbox {
    pub(super) fn show_crash_notice(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = take_crash_notice() {
            self.startup_crash = Some(path);
            let key = keymap::binding_text(&self.session.keymap, Action::OpenCrashReport);
            self.session.startup_crash_notice =
                Some(Toast::with_hint("Fell crashed last time", key, " to view"));
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(9)).await;
                let _ = this.update(cx, |inbox, cx| {
                    if inbox.session.startup_crash_notice.take().is_some() {
                        cx.notify();
                    }
                });
            })
            .detach();
            cx.notify();
        }
    }

    pub(super) fn watch_store_restarts(
        &self,
        mut notices: futures::channel::mpsc::UnboundedReceiver<WorkerEvent>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some(event) = notices.next().await {
                if this
                    .update(cx, |inbox, cx| inbox.apply_worker_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn apply_worker_event(&mut self, event: WorkerEvent, cx: &mut Context<Self>) {
        match event {
            WorkerEvent::Restarted(worker) => {
                let key = keymap::binding_text(&self.session.keymap, Action::OpenDiagnosticLog);
                self.session
                    .set_toast(Toast::with_hint(worker.restart_notice(), key, " for log"));
                let generation = self.session.toast_generation();
                self.worker_restart_notice = Some((worker, generation));
                self.dismiss_notice_after(generation, Duration::from_secs(5), cx);
            }
            WorkerEvent::Stopped(worker) => {
                if !self.session.worker_outages.insert(worker) {
                    return;
                }
                if let Some((owner, generation)) = self.worker_restart_notice
                    && owner == worker
                {
                    self.worker_restart_notice = None;
                    self.session.clear_toast_if(generation);
                }
            }
            WorkerEvent::Recovered(worker) => {
                self.session.worker_outages.remove(&worker);
            }
        }
        cx.notify();
    }

    fn diagnostic_notice(
        &mut self,
        text: impl Into<String>,
        duration: Duration,
        cx: &mut Context<Self>,
    ) {
        self.session.show_toast(text, true);
        self.dismiss_notice_after(self.session.toast_generation(), duration, cx);
        cx.notify();
    }

    pub(super) fn open_diagnostic(&mut self, file: DiagnosticFile, cx: &mut Context<Self>) {
        let startup = (file == DiagnosticFile::Crash)
            .then(|| self.startup_crash.take())
            .flatten();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let path = startup.map_or_else(|| file.path(), Ok)?;
                    open_in_editor(&path)
                })
                .await;
            if let Err(error) = result {
                fell_core::diagnostic!(
                    WARN,
                    "Could not open diagnostic file",
                    error = &error as &dyn std::error::Error
                );
                let notice = match (file, error.kind()) {
                    (DiagnosticFile::Crash, io::ErrorKind::NotFound) => "No crash report",
                    (DiagnosticFile::Crash, _) => "Could not open the crash report",
                    (DiagnosticFile::Log, _) => "Could not open the diagnostic log",
                };
                let _ = this.update(cx, |inbox, cx| {
                    inbox.diagnostic_notice(notice, Duration::from_secs(4), cx);
                });
            }
        })
        .detach();
    }
}

fn open_in_editor(path: &Path) -> io::Result<()> {
    if let Some(editor) = env::var_os("EDITOR")
        && let Some(args) = shlex::bytes::split(editor.as_bytes())
        && let Some((program, args)) = args.split_first()
    {
        let program = OsStr::from_bytes(program);
        let args = args.iter().map(|arg| OsStr::from_bytes(arg));
        let mut terminal = Command::new("xdg-terminal-exec");
        terminal.arg("--").arg(program).args(args.clone());
        if run_editor(terminal, path).is_ok() {
            return Ok(());
        }
        let mut command = Command::new(program);
        command.args(args);
        if run_editor(command, path).is_ok() {
            return Ok(());
        }
    }
    run_editor(Command::new("xdg-open"), path)
}

fn run_editor(mut command: Command, path: &Path) -> io::Result<()> {
    let status = command
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("editor exited unsuccessfully"))
    }
}
