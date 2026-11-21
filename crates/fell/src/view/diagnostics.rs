use std::env;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use fell_core::diagnostics::{DiagnosticFile, take_crash_notice};
use futures::StreamExt as _;
use gpui::Context;

use super::Inbox;
use crate::keymap::{self, Action};
use crate::worker::Worker;

impl Inbox {
    pub(super) fn show_crash_notice(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = take_crash_notice() {
            self.startup_crash = Some(path);
            let key = keymap::binding_text(&self.session.keymap, Action::OpenCrashReport);
            let key = key
                .strip_prefix("space ")
                .map_or_else(|| key.clone(), |s| format!("Space {s}"));
            self.diagnostic_notice(
                format!("Fell crashed last time · {key} to view"),
                Duration::from_secs(9),
                cx,
            );
        }
    }

    pub(super) fn watch_store_restarts(
        &self,
        mut notices: futures::channel::mpsc::UnboundedReceiver<Worker>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some(worker) = notices.next().await {
                if this
                    .update(cx, |inbox, cx| inbox.show_worker_restart(worker, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn show_worker_restart(&mut self, worker: Worker, cx: &mut Context<Self>) {
        self.diagnostic_notice(worker.notice(), Duration::from_secs(5), cx);
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
    let command = if let Some(editor) = env::var_os("EDITOR").filter(|value| !value.is_empty()) {
        let args = shlex::bytes::split(editor.as_encoded_bytes())
            .filter(|args| !args.is_empty())
            .ok_or_else(|| io::Error::other("invalid EDITOR command"))?;
        use std::os::unix::ffi::OsStringExt as _;
        let mut args = args.into_iter().map(std::ffi::OsString::from_vec);
        let program = args
            .next()
            .ok_or_else(|| io::Error::other("missing editor"))?;
        let mut terminal = Command::new("xdg-terminal-exec");
        terminal.arg("--").arg(&program).args(args.clone());
        match run_editor(terminal, path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            result => return result,
        }
        let mut command = Command::new(program);
        command.args(args);
        command
    } else {
        Command::new("xdg-open")
    };
    run_editor(command, path)
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
