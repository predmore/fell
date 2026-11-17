//! Compose window. The buffer is plain markdown. Styling is a view of that
//! text, through the same document model the conversation renderer uses.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use fell_core::compose::{
    BlockBody, DraftHub, HunkKind, Snapshot, StyleKind, StyledDoc, html_to_doc, line_styles,
    markdown_to_doc,
};
use fell_core::{AccountId, MailBackend};
use gpui::{
    Context, FocusHandle, FontWeight, KeyDownEvent, RenderImage, SharedString,
    StatefulInteractiveElement, Window, div, img, prelude::*, px, rgb,
};
use image::Frame;
use smallvec::SmallVec;

use crate::confirmation_from_ui;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ThemeKind {
    Dark,
    Light,
}

#[derive(Clone, Copy)]
struct Palette {
    bg: u32,
    fg: u32,
    dim: u32,
    marker: u32,
    accent: u32,
    caret: u32,
    code: u32,
    selection: u32,
    error: u32,
    fresh: u32,
    panel: u32,
    rule: u32,
    hunk: u32,
}

fn palette(kind: ThemeKind) -> Palette {
    match kind {
        ThemeKind::Dark => Palette {
            bg: 0x1c1b19,
            fg: 0xf4f0ea,
            dim: 0xe7e2db,
            marker: 0x8d8680,
            accent: 0x7aa2f7,
            caret: 0xffffff,
            code: 0x3a3834,
            selection: 0x2a3344,
            error: 0xf7768e,
            fresh: 0x2c2926,
            panel: 0x242320,
            rule: 0x6e675f,
            hunk: 0x243044,
        },
        ThemeKind::Light => Palette {
            bg: 0xf7f5f2,
            fg: 0x1c1b19,
            dim: 0x3a3632,
            marker: 0x6e675f,
            accent: 0x1d4f91,
            caret: 0x111111,
            code: 0xe4ddd4,
            selection: 0xd9e2f2,
            error: 0x9f1239,
            fresh: 0xf6eee4,
            panel: 0xfffdfb,
            rule: 0x8f877e,
            hunk: 0xe7eef8,
        },
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Insert,
    Visual,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    To,
    Cc,
    Bcc,
    Subject,
    Body,
}

struct Editor {
    lines: Vec<String>,
    row: usize,
    col: usize,
    mode: Mode,
    anchor: Option<(usize, usize)>,
    preferred: usize,
    count: usize,
}

impl Editor {
    fn from_body(body: &str) -> Self {
        let source = body.strip_suffix('\n').unwrap_or(body);
        let lines: Vec<String> = if source.is_empty() {
            vec![String::new()]
        } else {
            source.split('\n').map(str::to_string).collect()
        };
        let row = lines
            .iter()
            .position(|line| line.contains("Ship the"))
            .unwrap_or(0);
        let col = 0;
        Self {
            row,
            col,
            lines,
            mode: Mode::Normal,
            anchor: None,
            preferred: 0,
            count: 0,
        }
    }

    fn text(&self) -> String {
        let mut text = self.lines.join("\n");
        text.push('\n');
        text
    }

    fn clamp(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.row.min(self.lines.len() - 1);
        let len = self.lines[self.row].chars().count();
        self.col = self.col.min(len);
    }

    fn take_count(&mut self) -> usize {
        let count = self.count.max(1);
        self.count = 0;
        count
    }
}

pub struct ComposeView {
    hub: Arc<DraftHub>,
    draft_id: SharedString,
    folder: PathBuf,
    editor: Editor,
    applied_revision: u64,
    field: Field,
    to_line: String,
    cc_line: String,
    bcc_line: String,
    subject_line: String,
    field_col: usize,
    theme: ThemeKind,
    raw: bool,
    preview: bool,
    quote_open: bool,
    focus: FocusHandle,
    leader: bool,
    leader_at: Option<std::time::Instant>,
    which_visible: bool,
    palette: Option<String>,
    status: SharedString,
    sent: bool,
    /// A send is talking to the server. Another send waits for it.
    sending: bool,
    watching: bool,
    flush_pending: bool,
    flush_gen: u64,
}

impl ComposeView {
    pub fn new(
        hub: Arc<DraftHub>,
        draft_id: impl Into<String>,
        folder: PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let draft_id = draft_id.into();
        let snap = hub
            .with_session(&draft_id, |session| session.snapshot())
            .unwrap_or_else(|| empty_snapshot(&draft_id));
        let editor = Editor::from_body(&snap.body);
        Self {
            hub,
            draft_id: draft_id.into(),
            folder,
            editor,
            applied_revision: snap.revision,
            field: Field::Body,
            to_line: join_addrs(&snap.to),
            cc_line: join_addrs(&snap.cc),
            bcc_line: join_addrs(&snap.bcc),
            subject_line: snap.subject,
            field_col: 0,
            theme: ThemeKind::Dark,
            raw: false,
            preview: false,
            quote_open: false,
            focus: cx.focus_handle(),
            leader: false,
            leader_at: None,
            which_visible: false,
            palette: None,
            status: "i insert".into(),
            sent: false,
            sending: false,
            watching: false,
            flush_pending: false,
            flush_gen: 0,
        }
    }

    pub fn watch_live(&self, cx: &mut Context<Self>) {
        let rx = Arc::new(std::sync::Mutex::new(self.hub.subscribe()));
        cx.spawn(async move |this, cx| {
            loop {
                let rx = rx.clone();
                let ok = cx
                    .background_executor()
                    .spawn(async move { rx.lock().ok().is_some_and(|inbox| inbox.recv().is_ok()) })
                    .await;
                if !ok {
                    break;
                }
                if this
                    .update(cx, |this, cx| {
                        this.sync_from_hub();
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// Types into the paragraph so a recording can show the cursor line and an edit.
    pub fn play_edit(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1200))
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(row) = this
                    .editor
                    .lines
                    .iter()
                    .position(|line| line.contains("**ahead**"))
                {
                    this.editor.row = row;
                    this.editor.col = this.editor.lines[row].chars().count();
                    this.editor.mode = Mode::Insert;
                    this.status = "esc normal".into();
                }
                cx.notify();
            });
            cx.background_executor()
                .timer(Duration::from_millis(900))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.insert_str(" now");
                this.push_body();
                this.arm_debounced_flush(cx);
                this.status = "esc normal".into();
                cx.notify();
            });
            cx.background_executor()
                .timer(Duration::from_millis(800))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.editor.mode = Mode::Normal;
                this.end_insert();
                this.status = "i insert".into();
                cx.notify();
            });
        })
        .detach();
    }

    fn insert_str(&mut self, text: &str) {
        self.editor.clamp();
        let line = &mut self.editor.lines[self.editor.row];
        let at = byte_index(line, self.editor.col);
        line.insert_str(at, text);
        self.editor.col += text.chars().count();
    }

    pub fn set_theme(&mut self, theme: ThemeKind) {
        self.theme = theme;
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn sync_from_hub(&mut self) {
        self.follow_focus();
        if !self.pull_unpainted() {
            self.mark_painted();
        }
        self.hub.note_shown(self.draft_id.as_ref());
    }

    fn follow_focus(&mut self) {
        let Some(id) = self.hub.focused_id() else {
            return;
        };
        if id == self.draft_id.as_ref() {
            return;
        }
        if self.field != Field::Body {
            let _ = self.commit_field(self.field);
        }
        self.end_insert();
        let Some((folder, snap)) = self.hub.with_session(&id, |session| {
            session.open_for_edit();
            (session.folder().to_path_buf(), session.snapshot())
        }) else {
            return;
        };
        if folder != self.folder {
            release_previews(&self.folder);
        }
        self.folder = folder;
        self.draft_id = id.into();
        self.editor = Editor::from_body(&snap.body);
        self.applied_revision = snap.revision;
        self.field = Field::Body;
        self.to_line = join_addrs(&snap.to);
        self.cc_line = join_addrs(&snap.cc);
        self.bcc_line = join_addrs(&snap.bcc);
        self.subject_line = snap.subject;
        self.sent = false;
        self.quote_open = false;
        self.status = "i insert".into();
    }

    /// Copies session text that is not on screen. Returns true when the
    /// editor changed, which means this frame is the first time it is visible.
    fn pull_unpainted(&mut self) -> bool {
        let id = self.draft_id.to_string();
        let Some(snap) = self.hub.with_session(&id, |session| session.snapshot()) else {
            return false;
        };
        let mut changed = false;
        if canonical_body(&snap.body) != canonical_body(&self.editor.text()) {
            let row = self.editor.row;
            let col = self.editor.col;
            let mode = self.editor.mode;
            self.editor = Editor::from_body(&snap.body);
            self.editor.row = row;
            self.editor.col = col;
            self.editor.mode = mode;
            self.editor.clamp();
            // Text from outside starts a new undo step for the next keystroke.
            self.end_insert();
            changed = true;
        }
        if self.field != Field::To {
            let line = join_addrs(&snap.to);
            if self.to_line != line {
                self.to_line = line;
                changed = true;
            }
        }
        if self.field != Field::Cc {
            let line = join_addrs(&snap.cc);
            if self.cc_line != line {
                self.cc_line = line;
                changed = true;
            }
        }
        if self.field != Field::Bcc {
            let line = join_addrs(&snap.bcc);
            if self.bcc_line != line {
                self.bcc_line = line;
                changed = true;
            }
        }
        if self.field != Field::Subject && self.subject_line != snap.subject {
            self.subject_line = snap.subject;
            changed = true;
        }
        self.applied_revision = snap.revision;
        changed
    }

    fn mark_painted(&mut self) {
        let id = self.draft_id.to_string();
        let _ = self.hub.with_session(&id, |session| {
            if canonical_body(session.body()) == canonical_body(&self.editor.text()) {
                session.mark_seen();
            }
        });
    }

    fn push_body(&mut self) {
        let body = self.editor.text();
        let mode = self.editor.mode;
        let id = self.draft_id.to_string();
        if let Some(revision) = self.hub.with_session(&id, |session| {
            record_body(session, body, mode);
            session.arm_outbound(now_ms());
            session.revision()
        }) {
            self.applied_revision = revision;
            self.flush_pending = true;
        }
    }

    /// Closes the undo group an insert burst kept open.
    fn end_insert(&mut self) {
        let id = self.draft_id.to_string();
        let _ = self
            .hub
            .with_session(&id, |session| session.end_user_change());
    }

    fn arm_debounced_flush(&mut self, cx: &mut Context<Self>) {
        if !self.flush_pending {
            return;
        }
        self.flush_pending = false;
        self.flush_gen = self.flush_gen.wrapping_add(1);
        let generation = self.flush_gen;
        let id = self.draft_id.to_string();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let _ = this.update(cx, |this, _cx| {
                if this.flush_gen != generation {
                    return;
                }
                this.hub.with_session(&id, |session| {
                    session.poll_outbound(now_ms());
                });
            });
        })
        .detach();
    }

    fn flush_now(&mut self) {
        let id = self.draft_id.to_string();
        self.flush_pending = false;
        self.flush_gen = self.flush_gen.wrapping_add(1);
        self.hub.with_session(&id, |session| {
            let _ = session.flush();
        });
    }

    fn restore_header_row(&mut self, field: Field) {
        let id = self.draft_id.to_string();
        let Some(snap) = self.hub.with_session(&id, |session| session.snapshot()) else {
            return;
        };
        match field {
            Field::To => self.to_line = join_addrs(&snap.to),
            Field::Cc => self.cc_line = join_addrs(&snap.cc),
            Field::Bcc => self.bcc_line = join_addrs(&snap.bcc),
            Field::Subject => self.subject_line = snap.subject.clone(),
            Field::Body => {}
        }
    }

    fn commit_field(&mut self, field: Field) -> bool {
        let id = self.draft_id.to_string();
        let result = self.hub.with_session(&id, |session| match field {
            Field::To => session.set_addresses_from_user("to", &self.to_line),
            Field::Cc => session.set_addresses_from_user("cc", &self.cc_line),
            Field::Bcc => session.set_addresses_from_user("bcc", &self.bcc_line),
            Field::Subject => {
                session.set_subject_from_user(self.subject_line.clone());
                Ok(())
            }
            Field::Body => Ok(()),
        });
        match result {
            Some(Ok(())) => {
                let _ = self.hub.with_session(&id, |session| session.flush());
                if self.pull_unpainted() {
                    self.status = "the draft changed; it is on screen before send".into();
                    return false;
                }
                self.mark_painted();
                self.status = "draft".into();
                true
            }
            Some(Err(err)) => {
                self.status = err.into();
                false
            }
            None => false,
        }
    }

    /// Check and build the message under the draft lock, then talk to SMTP
    /// on a background thread. The window keeps painting while Bridge
    /// answers, and a second send waits for the first to finish.
    fn send(&mut self, cx: &mut Context<Self>) {
        if self.sending {
            self.status = "Sending…".into();
            return;
        }
        if self.field != Field::Body && !self.commit_field(self.field) {
            return;
        }
        if self.pull_unpainted() {
            self.status = "the draft changed; it is on screen before send".into();
            return;
        }
        self.flush_now();
        let id = self.draft_id.to_string();
        let prepared = self.hub.with_session(&id, prepare_from_editor);
        let prepared = match prepared {
            Some(Ok(prepared)) => prepared,
            Some(Err(err)) => {
                self.sent = false;
                self.status = format!("Not sent: {err}").into();
                return;
            }
            None => {
                self.status = "draft is not open".into();
                return;
            }
        };
        self.sending = true;
        self.status = "Sending…".into();
        let hub = self.hub.clone();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    prepared.transmit(|| hub.with_session(&id, |session| session.revision()))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.sending = false;
                match outcome {
                    Ok(_) => {
                        this.sent = true;
                        this.status = "Sent from the editor".into();
                    }
                    Err(err) => {
                        this.sent = false;
                        this.status = format!("Not sent: {err}").into();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn undo(&mut self) {
        let id = self.draft_id.to_string();
        if self.hub.with_session(&id, |session| session.undo()) == Some(true) {
            self.applied_revision = 0;
            self.sync_from_hub();
            self.status = "undone".into();
        }
    }

    fn keep_or_reject(&mut self, keep: bool) {
        let id = self.draft_id.to_string();
        let hunk = self.hub.with_session(&id, |session| {
            session.snapshot().hunks.first().map(|hunk| hunk.id)
        });
        if let Some(Some(hunk_id)) = hunk {
            self.hub.with_session(&id, |session| {
                if keep {
                    session.keep_hunk(hunk_id);
                } else {
                    session.reject_hunk(hunk_id);
                }
                let _ = session.flush();
            });
            self.applied_revision = 0;
            self.sync_from_hub();
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let key = event.keystroke.key.as_str();
        let typed =
            event.keystroke.key_char.as_deref().filter(|_| {
                !event.keystroke.modifiers.control && !event.keystroke.modifiers.platform
            });
        if self.palette.is_some() {
            self.palette_key(key, typed, cx);
            return;
        }
        if self.leader {
            self.leader_key(key, cx);
            return;
        }
        if key == "escape" {
            if self.field != Field::Body {
                let field = self.field;
                let committed = self.commit_field(field);
                if !committed {
                    self.restore_header_row(field);
                }
                self.editor.mode = Mode::Normal;
                self.editor.anchor = None;
                self.field = Field::Body;
                if committed {
                    self.status = "normal".into();
                }
                return;
            }
            self.editor.mode = Mode::Normal;
            self.editor.anchor = None;
            self.field = Field::Body;
            self.end_insert();
            self.status = "normal".into();
            return;
        }
        if self.field != Field::Body {
            self.field_key(key, typed);
            return;
        }
        if event.keystroke.modifiers.control && key == "enter" {
            return;
        }
        match self.editor.mode {
            Mode::Insert => self.insert_key(key, typed),
            Mode::Normal => self.normal_key(key, typed),
            Mode::Visual => self.visual_key(key),
        }
        if self.flush_pending {
            self.arm_debounced_flush(cx);
        }
    }

    fn palette_key(&mut self, key: &str, typed: Option<&str>, cx: &mut Context<Self>) {
        let Some(query) = self.palette.as_mut() else {
            return;
        };
        match key {
            "escape" => self.palette = None,
            "enter" => {
                let query = query.clone();
                self.palette = None;
                self.run_palette(&query, cx);
            }
            "backspace" => {
                query.pop();
            }
            _ => {
                if let Some(ch) = typed {
                    query.push_str(ch);
                }
            }
        }
    }

    fn run_palette(&mut self, query: &str, cx: &mut Context<Self>) {
        if let Some(action) = palette_action(query) {
            self.run_action(action, cx);
        }
    }

    fn run_action(&mut self, action: &str, cx: &mut Context<Self>) {
        match action {
            "send" => self.send(cx),
            "preview" => {
                self.preview = !self.preview;
                self.status = if self.preview {
                    "html preview"
                } else {
                    "editor"
                }
                .into();
            }
            "raw" => {
                self.raw = !self.raw;
                self.status = if self.raw { "raw markdown" } else { "styled" }.into();
            }
            "quote" => self.toggle_quote(),
            "theme" => {
                self.theme = match self.theme {
                    ThemeKind::Dark => ThemeKind::Light,
                    ThemeKind::Light => ThemeKind::Dark,
                };
            }
            "keep" => self.keep_or_reject(true),
            "reject" => self.keep_or_reject(false),
            "undo" => self.undo(),
            _ => {}
        }
    }

    fn leader_key(&mut self, key: &str, cx: &mut Context<Self>) {
        match key {
            "m" => {
                self.status = "mail: s send · p preview · r raw · q quote · t theme".into();
                return;
            }
            "a" => {
                self.status = "agent: k keep · x reject".into();
                return;
            }
            "s" | "p" | "r" | "q" | "t" | "k" | "x" => {}
            "escape" => {
                self.leader = false;
                self.which_visible = false;
                return;
            }
            _ => {
                self.leader = false;
                self.which_visible = false;
                return;
            }
        }
        let status = self.status.to_string();
        let action = if status.starts_with("mail") {
            match key {
                "s" => "send",
                "p" => "preview",
                "r" => "raw",
                "q" => "quote",
                "t" => "theme",
                _ => "",
            }
        } else if status.starts_with("agent") {
            match key {
                "k" => "keep",
                "x" => "reject",
                _ => "",
            }
        } else {
            ""
        };
        self.leader = false;
        self.which_visible = false;
        if !action.is_empty() {
            self.run_action(action, cx);
        }
    }

    fn field_key(&mut self, key: &str, typed: Option<&str>) {
        let line = match self.field {
            Field::To => &mut self.to_line,
            Field::Cc => &mut self.cc_line,
            Field::Bcc => &mut self.bcc_line,
            Field::Subject => &mut self.subject_line,
            Field::Body => return,
        };
        match key {
            "backspace" => {
                line.pop();
            }
            "tab" | "enter" => {
                let field = self.field;
                if self.commit_field(field) {
                    self.field = match self.field {
                        Field::To => Field::Cc,
                        Field::Cc => Field::Bcc,
                        Field::Bcc => Field::Subject,
                        _ => Field::Body,
                    };
                }
            }
            "j" | "k" if typed.is_none() => {}
            _ => {
                if let Some(ch) = typed {
                    line.push_str(ch);
                }
            }
        }
        let _ = self.field_col;
    }

    fn insert_key(&mut self, key: &str, typed: Option<&str>) {
        match key {
            "escape" => {
                self.editor.mode = Mode::Normal;
                self.push_body();
                self.end_insert();
                self.status = "normal".into();
            }
            "backspace" => {
                self.editor.clamp();
                if self.editor.col == 0 {
                    if self.editor.row > 0 {
                        let current = self.editor.lines.remove(self.editor.row);
                        self.editor.row -= 1;
                        self.editor.col = self.editor.lines[self.editor.row].chars().count();
                        self.editor.lines[self.editor.row].push_str(&current);
                    }
                } else {
                    let line = &mut self.editor.lines[self.editor.row];
                    let byte = byte_index(line, self.editor.col - 1);
                    let next = byte_index(line, self.editor.col);
                    line.replace_range(byte..next, "");
                    self.editor.col -= 1;
                }
                self.push_body();
            }
            "enter" => {
                self.editor.clamp();
                let line = self.editor.lines[self.editor.row].clone();
                let byte = byte_index(&line, self.editor.col);
                let (left, right) = line.split_at(byte);
                self.editor.lines[self.editor.row] = left.to_string();
                self.editor.row += 1;
                self.editor.lines.insert(self.editor.row, right.to_string());
                self.editor.col = 0;
                self.push_body();
            }
            _ => {
                if let Some(ch) = typed {
                    self.editor.clamp();
                    let line = &mut self.editor.lines[self.editor.row];
                    let byte = byte_index(line, self.editor.col);
                    line.insert_str(byte, ch);
                    self.editor.col += ch.chars().count();
                    self.push_body();
                }
            }
        }
    }

    fn normal_key(&mut self, key: &str, typed: Option<&str>) {
        if key != "0"
            && key.chars().all(|ch| ch.is_ascii_digit())
            && let Ok(digit) = key.parse::<usize>()
        {
            self.editor.count = self.editor.count.saturating_mul(10).saturating_add(digit);
            return;
        }
        match key {
            " " => {
                self.leader = true;
                self.leader_at = Some(std::time::Instant::now());
                self.which_visible = false;
                self.status = "space".into();
            }
            ":" => self.palette = Some(String::new()),
            "i" => {
                self.editor.mode = Mode::Insert;
                self.status = "insert".into();
            }
            "a" => {
                self.editor.clamp();
                let len = self.editor.lines[self.editor.row].chars().count();
                if self.editor.col < len {
                    self.editor.col += 1;
                }
                self.editor.mode = Mode::Insert;
                self.status = "insert".into();
            }
            "o" => {
                self.editor.row += 1;
                self.editor.lines.insert(self.editor.row, String::new());
                self.editor.col = 0;
                self.editor.mode = Mode::Insert;
                self.push_body();
            }
            "h" | "left" => self.move_col(-1),
            "l" | "right" => self.move_col(1),
            "j" | "down" => self.move_row(1),
            "k" | "up" => self.move_row(-1),
            "0" => {
                self.editor.col = 0;
                self.editor.preferred = 0;
            }
            "$" => {
                self.editor.clamp();
                self.editor.col = self.editor.lines[self.editor.row].chars().count();
            }
            "w" => self.word(true),
            "b" => self.word(false),
            "g" => {
                self.editor.row = 0;
                self.editor.col = 0;
            }
            "G" => {
                self.editor.row = self.editor.lines.len().saturating_sub(1);
                self.editor.col = 0;
            }
            "x" => {
                self.editor.clamp();
                let line = &mut self.editor.lines[self.editor.row];
                if self.editor.col < line.chars().count() {
                    let start = byte_index(line, self.editor.col);
                    let end = byte_index(line, self.editor.col + 1);
                    line.replace_range(start..end, "");
                    self.push_body();
                }
            }
            "u" => self.undo(),
            "v" => {
                self.editor.anchor = Some((self.editor.row, self.editor.col));
                self.editor.mode = Mode::Visual;
            }
            "tab" => {
                self.field = Field::To;
                self.status = "to".into();
            }
            _ => {
                let _ = typed;
            }
        }
    }

    fn visual_key(&mut self, key: &str) {
        match key {
            "escape" => {
                self.editor.mode = Mode::Normal;
                self.editor.anchor = None;
            }
            "h" | "l" | "j" | "k" => self.normal_key(key, None),
            "d" | "x" => {
                if let Some((row, col)) = self.editor.anchor {
                    let start = (row.min(self.editor.row), col.min(self.editor.col));
                    let end = (row.max(self.editor.row), col.max(self.editor.col));
                    if start.0 == end.0 {
                        let line = &mut self.editor.lines[start.0];
                        let from = byte_index(line, start.1);
                        let to = byte_index(line, end.1);
                        if from < to && to <= line.len() {
                            line.replace_range(from..to, "");
                        }
                    }
                    self.editor.row = start.0;
                    self.editor.col = start.1;
                    self.editor.mode = Mode::Normal;
                    self.editor.anchor = None;
                    self.push_body();
                }
            }
            _ => {}
        }
    }

    fn move_col(&mut self, delta: isize) {
        let count = self.editor.take_count() as isize;
        self.editor.clamp();
        let next = self.editor.col as isize + delta * count;
        self.editor.col = next.max(0) as usize;
        self.editor.clamp();
        self.editor.preferred = self.editor.col;
    }

    fn move_row(&mut self, delta: isize) {
        let count = self.editor.take_count() as isize;
        self.editor.clamp();
        let next = self.editor.row as isize + delta * count;
        self.editor.row = next.max(0) as usize;
        self.editor.col = self.editor.preferred;
        self.editor.clamp();
    }

    fn word(&mut self, forward: bool) {
        self.editor.clamp();
        let line = &self.editor.lines[self.editor.row];
        let chars: Vec<char> = line.chars().collect();
        if forward {
            let mut index = self.editor.col.min(chars.len());
            while index < chars.len() && chars[index].is_whitespace() {
                index += 1;
            }
            while index < chars.len() && !chars[index].is_whitespace() {
                index += 1;
            }
            self.editor.col = index;
        } else if self.editor.col > 0 {
            let mut index = self.editor.col - 1;
            while index > 0 && chars[index].is_whitespace() {
                index -= 1;
            }
            while index > 0 && !chars[index - 1].is_whitespace() {
                index -= 1;
            }
            self.editor.col = index;
        }
        self.editor.preferred = self.editor.col;
    }
}

impl Render for ComposeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_from_hub();
        if self.leader && !self.which_visible {
            let ready = self
                .leader_at
                .is_some_and(|at| at.elapsed() >= Duration::from_millis(300));
            if ready {
                self.which_visible = true;
            } else if !self.watching {
                self.watching = true;
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(300))
                        .await;
                    this.update(cx, |this, cx| {
                        this.watching = false;
                        if this.leader {
                            this.which_visible = true;
                        }
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            }
        }
        window.focus(&self.focus);
        let snap = self
            .hub
            .with_session(self.draft_id.as_ref(), |session| session.snapshot())
            .unwrap_or_else(|| empty_snapshot(self.draft_id.as_ref()));
        let colors = palette(self.theme);
        let mode = match self.editor.mode {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Visual => "VISUAL",
        };
        let hint: SharedString = match (self.editor.mode, self.status.as_ref()) {
            (Mode::Insert, "i insert" | "insert") => "esc normal".into(),
            (Mode::Normal, "esc normal") => "i insert".into(),
            _ => self.status.clone(),
        };
        let unreviewed = snap.unreviewed;
        let wide = window.viewport_size().width > px(640.);
        let send_label = if let Some(reason) = &snap.send_block {
            format!("send blocked · {reason}")
        } else if unreviewed == 0 {
            "send space m s".to_string()
        } else {
            format!("send space m s · {unreviewed} unreviewed")
        };
        div()
            .id("compose")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| this.on_key(event, cx)))
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(rgb(colors.bg))
            .text_color(rgb(colors.fg))
            .text_size(px(16.))
            .font_weight(FontWeight(400.))
            .font_family("Liberation Sans")
            .child(header(&snap, colors).flex_shrink_0())
            .child(self.fields_view(&snap, colors).flex_shrink_0())
            .child(self.body_view(&snap, colors))
            .children(
                self.quote_view(&snap, colors)
                    .map(|quote| quote.flex_shrink_0()),
            )
            .child(attachment_row(&snap, colors).flex_shrink_0())
            .child(status_bar(mode, &send_label, &hint, &snap, colors, wide).flex_shrink_0())
            .children(self.which_visible.then(|| which_key(colors)))
            .children(
                self.palette
                    .as_ref()
                    .map(|query| palette_view(query, colors)),
            )
            .children(self.preview.then(|| preview_layer(&snap, colors)))
    }
}

impl ComposeView {
    fn fields_view(&self, snap: &Snapshot, colors: Palette) -> gpui::Div {
        let mut column = div().flex().flex_col().bg(rgb(colors.panel)).px_4().py_1();
        column = field_row(
            column,
            "To",
            &self.to_line,
            self.field == Field::To,
            &snap.to,
            colors,
        );
        column = field_row(
            column,
            "Cc",
            &self.cc_line,
            self.field == Field::Cc,
            &snap.cc,
            colors,
        );
        column = field_row(
            column,
            "Bcc",
            &self.bcc_line,
            self.field == Field::Bcc,
            &snap.bcc,
            colors,
        );
        column
            .child(subject_row(
                &self.subject_line,
                self.field == Field::Subject,
                colors,
            ))
            .child(if snap.hunks.is_empty() {
                reply_row(snap, colors)
            } else {
                hunk_banner(snap, colors)
            })
    }

    fn body_view(&self, snap: &Snapshot, colors: Palette) -> gpui::Div {
        let body = div()
            .id("compose-body")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .px_5()
            .py_2()
            .child(if self.raw {
                raw_body(&self.editor, colors)
            } else {
                styled_body(&self.editor, &self.folder, &body_edit_ranges(snap), colors)
            });
        let mut column = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .overflow_hidden();
        for issue in &snap.issues {
            column = column.child(
                div()
                    .px_5()
                    .text_sm()
                    .text_color(rgb(colors.error))
                    .child(format!("line {}: {}", issue.line, issue.message)),
            );
        }
        for notice in &snap.notices {
            column = column.child(
                div()
                    .px_5()
                    .text_sm()
                    .text_color(rgb(colors.dim))
                    .child(notice.clone()),
            );
        }
        column.child(body)
    }

    /// The quoted part stored on the draft. Absent when nothing is quoted.
    fn quote_view(&self, snap: &Snapshot, colors: Palette) -> Option<gpui::Div> {
        let html = stored_quote(snap)?;
        let label = if self.quote_open {
            "Quoted original"
        } else {
            "Quoted original  ·  space m q"
        };
        let mut block = div()
            .px_5()
            .py_2()
            .text_size(px(16.))
            .text_color(rgb(colors.fg))
            .border_t_1()
            .border_color(rgb(colors.rule))
            .child(label);
        if self.quote_open {
            block = block.child(render_doc(&html_to_doc(html), &self.folder, colors));
        }
        Some(block)
    }

    fn toggle_quote(&mut self) {
        let id = self.draft_id.to_string();
        let quoted = self
            .hub
            .with_session(&id, |session| stored_quote(&session.snapshot()).is_some())
            .unwrap_or(false);
        if quoted {
            self.quote_open = !self.quote_open;
        } else {
            self.quote_open = false;
            self.status = "nothing quoted".into();
        }
    }
}

impl Drop for ComposeView {
    fn drop(&mut self) {
        release_previews(&self.folder);
    }
}

/// Check and send the draft on screen in one call. Blocks on SMTP, so the
/// window uses [`prepare_from_editor`] and transmits off its thread.
pub fn send_from_editor(
    session: &mut fell_core::compose::DraftSession,
    backend: &dyn MailBackend,
) -> Result<fell_core::SentMessage, String> {
    prepare_from_editor(session)?.transmit_with(backend)
}

/// The message the editor is about to send, checked and built while the
/// draft lock is held, so what was on screen is what goes out.
pub struct PreparedSend {
    account: fell_core::AccountId,
    /// The draft revision these checks passed on.
    revision: u64,
    outgoing: fell_core::compose::Outgoing,
    files: Vec<fell_core::SubmittedFile>,
}

/// Run the editor's send checks and read the attachments. No network.
pub fn prepare_from_editor(
    session: &mut fell_core::compose::DraftSession,
) -> Result<PreparedSend, String> {
    let outgoing = session.prepare_send()?;
    let files = session
        .resolved_files()
        .into_iter()
        .map(|file| fell_core::SubmittedFile {
            name: file.display_name,
            bytes: file.bytes,
            inline: file.inline,
        })
        .collect();
    Ok(PreparedSend {
        account: session.account_id().clone(),
        revision: session.revision(),
        outgoing,
        files,
    })
}

impl PreparedSend {
    /// Submit and confirm through the account's backend. Blocks on SMTP.
    /// `revision_now` reads the draft's revision right before the message is
    /// handed to SMTP; if anything changed since the checks, nothing is sent.
    pub fn transmit(
        self,
        revision_now: impl FnOnce() -> Option<u64>,
    ) -> Result<fell_core::SentMessage, String> {
        let backend = fell_core::backend_for(&self.account);
        self.transmit_checked(backend.as_ref(), revision_now)
    }

    fn transmit_with(self, backend: &dyn MailBackend) -> Result<fell_core::SentMessage, String> {
        let revision = self.revision;
        self.transmit_checked(backend, || Some(revision))
    }

    fn transmit_checked(
        self,
        backend: &dyn MailBackend,
        revision_now: impl FnOnce() -> Option<u64>,
    ) -> Result<fell_core::SentMessage, String> {
        let pending = backend
            .submit_composed(self.outgoing.draft, self.outgoing.mime, self.files)
            .map_err(|err| err.to_string())?;
        if revision_now() != Some(self.revision) {
            return Err("the draft changed while sending; nothing was sent".into());
        }
        let confirm = confirmation_from_ui(&pending);
        backend
            .confirm_send(pending, confirm)
            .map_err(|err| err.to_string())
    }
}

fn header(snap: &Snapshot, colors: Palette) -> gpui::Div {
    div()
        .flex()
        .justify_between()
        .px_4()
        .py_2()
        .bg(rgb(colors.panel))
        .border_b_1()
        .border_color(rgb(colors.selection))
        .child(div().font_weight(FontWeight(620.)).child("Compose"))
        .child(
            div()
                .text_size(px(16.))
                .text_color(rgb(colors.dim))
                .child(format!("{}   {}", snap.from, snap.id)),
        )
}

fn field_row(
    parent: gpui::Div,
    label: &str,
    value: &str,
    active: bool,
    addresses: &[fell_core::compose::AddressView],
    colors: Palette,
) -> gpui::Div {
    let fresh = addresses.iter().any(|address| address.fresh);
    let bg = if fresh { colors.fresh } else { colors.panel };
    parent.child(
        div()
            .flex()
            .gap_3()
            .px_1()
            .py_1()
            .bg(rgb(bg))
            .border_l(px(2.))
            .border_color(rgb(if active || fresh { colors.accent } else { bg }))
            .child(
                div()
                    .w(px(88.))
                    .text_size(px(16.))
                    .text_color(rgb(colors.fg))
                    .child(label.to_string()),
            )
            .child(div().flex_1().child(if value.is_empty() {
                SharedString::from(" ")
            } else {
                SharedString::from(value.to_string())
            }))
            .children(fresh.then(|| {
                div()
                    .text_size(px(16.))
                    .text_color(rgb(colors.accent))
                    .child("new")
            })),
    )
}

fn subject_row(subject: &str, active: bool, colors: Palette) -> gpui::Div {
    div()
        .flex()
        .gap_3()
        .px_1()
        .py_1()
        .border_l(px(2.))
        .border_color(rgb(if active { colors.accent } else { colors.panel }))
        .child(
            div()
                .w(px(88.))
                .text_size(px(16.))
                .text_color(rgb(colors.fg))
                .child("Subject"),
        )
        .child(
            div()
                .font_weight(FontWeight(560.))
                .child(subject.to_string()),
        )
}

fn reply_row(snap: &Snapshot, colors: Palette) -> gpui::Div {
    let reply = snap
        .in_reply_to
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or("new message");
    div()
        .flex()
        .gap_3()
        .px_1()
        .py_1()
        .border_l(px(2.))
        .border_color(rgb(colors.panel))
        .text_size(px(16.))
        .text_color(rgb(colors.fg))
        .child(div().w(px(88.)).child("Reply to"))
        .child(reply.to_string())
}

fn hunk_banner(snap: &Snapshot, colors: Palette) -> gpui::Div {
    let label = snap
        .hunks
        .first()
        .map(|hunk| match hunk.kind {
            HunkKind::Recipients => "agent changed recipients",
            HunkKind::Subject => "agent changed the subject",
            HunkKind::Attachments => "agent changed attachments",
            HunkKind::Body => "agent edit",
        })
        .unwrap_or("agent edit");
    div()
        .flex()
        .px_1()
        .py_1()
        .border_l(px(2.))
        .border_color(rgb(colors.hunk))
        .bg(rgb(colors.hunk))
        .text_size(px(16.))
        .text_color(rgb(colors.fg))
        .child(format!(
            "{label}  ·  u undo  ·  space a k keep  ·  space a x reject"
        ))
}

fn body_edit_ranges(snap: &Snapshot) -> Vec<(usize, usize)> {
    snap.hunks
        .iter()
        .filter(|hunk| hunk.kind == HunkKind::Body)
        .filter_map(|hunk| {
            let slice = snap.text.get(hunk.start..hunk.end)?.trim();
            if slice.is_empty() {
                return None;
            }
            let at = snap.body.find(slice)?;
            Some((at, at + slice.len()))
        })
        .collect()
}

fn styled_body(
    editor: &Editor,
    folder: &Path,
    hunks: &[(usize, usize)],
    colors: Palette,
) -> gpui::Div {
    let text = editor.text();
    let doc = markdown_to_doc(&text);
    let mut column = div().flex().flex_col();
    let mut previous: Option<&BlockBody> = None;
    for (index, item) in body_layout(&text, editor.row, editor.col, &doc)
        .into_iter()
        .enumerate()
    {
        let (body, mut painted) = match item {
            BodyItem::CaretRow => (None, cursor_line_row(editor, colors)),
            BodyItem::Block { index, raw } => {
                let block = &doc.blocks[index];
                let edited = block.source.is_some_and(|(start, end)| {
                    hunks
                        .iter()
                        .any(|(hunk_start, hunk_end)| start < *hunk_end && end > *hunk_start)
                });
                let painted = if raw {
                    raw_source_block(block, &text, editor, colors)
                } else {
                    render_block(block, folder, colors)
                };
                let painted = if edited {
                    div().w_full().bg(rgb(colors.hunk)).child(painted)
                } else {
                    painted
                };
                // A block in source form spaces like text, not like an image.
                ((!raw).then_some(&block.body), painted)
            }
        };
        if index > 0 && !joins_previous(previous, body) {
            painted = painted.mt_2();
        }
        previous = body;
        column = column.child(painted);
    }
    column
}

/// One row of the styled body, top to bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyItem {
    /// The cursor line when no block covers it, such as a blank line.
    CaretRow,
    /// A block of the document, in source form when it holds the cursor.
    Block { index: usize, raw: bool },
}

fn body_layout(text: &str, row: usize, col: usize, doc: &StyledDoc) -> Vec<BodyItem> {
    let cursor_at_col = cursor_byte(text, row, col);
    let cursor_at = cursor_byte(text, row, 0);
    let mut caret_placed = cursor_line_covered(text, row, doc);
    let mut items = Vec::with_capacity(doc.blocks.len() + 1);
    for (index, block) in doc.blocks.iter().enumerate() {
        if !caret_placed && block.source.is_some_and(|(start, _)| cursor_at < start) {
            items.push(BodyItem::CaretRow);
            caret_placed = true;
        }
        let raw = match block.source {
            Some(range) => block_holds_cursor(range, cursor_at_col, text.len()),
            None => row == 0,
        };
        items.push(BodyItem::Block { index, raw });
    }
    if !caret_placed {
        items.push(BodyItem::CaretRow);
    }
    items
}

/// A block's source range runs up to the first byte of the next line, so its
/// end is exclusive: the cursor there belongs to the next line. The end byte
/// counts only at the end of the buffer, where no next line exists.
fn block_holds_cursor((start, end): (usize, usize), cursor: usize, len: usize) -> bool {
    cursor >= start && (cursor < end || cursor == end && end == len)
}

/// An image directly under a quote sits against it, without the block gap.
fn joins_previous(previous: Option<&BlockBody>, next: Option<&BlockBody>) -> bool {
    matches!(
        (previous, next),
        (Some(BlockBody::Quote { .. }), Some(BlockBody::Image { .. }))
    )
}

fn cursor_line_covered(text: &str, row: usize, doc: &StyledDoc) -> bool {
    let start = cursor_byte(text, row, 0);
    let line = text.split_inclusive('\n').nth(row).unwrap_or("");
    if line.is_empty() {
        return false;
    }
    let end = start + line.len();
    doc.blocks.iter().any(|block| {
        block
            .source
            .is_some_and(|(block_start, block_end)| start < block_end && end > block_start)
    })
}

fn cursor_line_row(editor: &Editor, colors: Palette) -> gpui::Div {
    let line = editor
        .lines
        .get(editor.row)
        .map(String::as_str)
        .unwrap_or("");
    raw_line(line, Some(editor.col), true, editor.mode, colors)
}

fn raw_body(editor: &Editor, colors: Palette) -> gpui::Div {
    let mut column = div().flex().flex_col().font_family("monospace");
    for (index, line) in editor.lines.iter().enumerate() {
        let cursor = (editor.mode != Mode::Normal || true) && index == editor.row;
        column = column.child(raw_line(
            line,
            cursor.then_some(editor.col),
            true,
            editor.mode,
            colors,
        ));
    }
    column
}

fn raw_source_block(
    block: &fell_core::compose::Block,
    text: &str,
    editor: &Editor,
    colors: Palette,
) -> gpui::Div {
    let Some((start, end)) = block.source else {
        return render_block(block, Path::new(""), colors);
    };
    let slice = &text[start.min(text.len())..end.min(text.len())];
    let mut column = div().flex().flex_col();
    let base_line = text[..start].chars().filter(|ch| *ch == '\n').count();
    for (offset, line) in slice.trim_end_matches('\n').split('\n').enumerate() {
        let row = base_line + offset;
        let cursor = (row == editor.row).then_some(editor.col);
        column = column.child(raw_line(
            line,
            cursor,
            cursor.is_some(),
            editor.mode,
            colors,
        ));
    }
    column
}

fn raw_line(
    line: &str,
    cursor: Option<usize>,
    show_markers: bool,
    mode: Mode,
    colors: Palette,
) -> gpui::Div {
    let line_chars = line.chars().count();
    let cursor = match (mode, cursor) {
        (Mode::Insert, col) => col,
        (_, Some(col)) if line_chars > 0 && col >= line_chars => Some(line_chars - 1),
        (_, col) => col,
    };
    let styles = line_styles(line);
    let mut row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .w_full()
        .min_h(px(24.))
        .items_center();
    if styles.is_empty() {
        if cursor.is_some() {
            row = row.child(caret(colors, mode));
        }
        return row;
    }
    let cursor_byte = cursor.map(|col| byte_index(line, col));
    for span in styles {
        if !show_markers && span.kind == StyleKind::Marker {
            if span.start == 0 {
                row = row.child(folded_marker(line, colors));
            }
            continue;
        }
        let slice = &line[span.start.min(line.len())..span.end.min(line.len())];
        if span.start == 0
            && span.kind == StyleKind::Marker
            && matches!(slice, "- " | "-" | "* " | "*")
        {
            row = row.child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight(400.))
                    .text_color(rgb(colors.marker))
                    .pr(px(6.))
                    .child(slice.trim().to_string()),
            );
            continue;
        }
        let color = match span.kind {
            StyleKind::Marker | StyleKind::LinkUrl => colors.marker,
            StyleKind::LinkText => colors.accent,
            _ => colors.fg,
        };
        let weight = match span.kind {
            StyleKind::Strong => FontWeight(700.),
            _ => FontWeight(400.),
        };
        if let Some(cursor_byte) = cursor_byte
            && cursor_byte >= span.start
            && cursor_byte < span.end
        {
            let at = cursor_byte - span.start;
            let (left, right) = split_at_char_boundary(slice, at);
            if !left.is_empty() {
                row = row.child(styled_span(left, color, weight, span.kind, colors.code));
            }
            if mode != Mode::Insert
                && let Some(ch) = right.chars().next()
            {
                let split = ch.len_utf8();
                let (glyph, rest) = right.split_at(split);
                row = row.child(inverted_span(glyph, weight, span.kind, colors));
                if !rest.is_empty() {
                    row = row.child(styled_span(rest, color, weight, span.kind, colors.code));
                }
            } else {
                row = row.child(caret(colors, mode));
                if !right.is_empty() {
                    row = row.child(styled_span(right, color, weight, span.kind, colors.code));
                }
            }
            continue;
        }
        row = row.child(styled_span(slice, color, weight, span.kind, colors.code));
    }
    if cursor_byte.is_some_and(|at| at >= line.len()) {
        row = row.child(caret(colors, mode));
    }
    row
}

fn folded_marker(line: &str, colors: Palette) -> gpui::Div {
    let trimmed = line.trim_start();
    let indent = " ".repeat(line.len() - trimmed.len());
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        return div()
            .text_size(px(18.))
            .font_weight(FontWeight(700.))
            .text_color(rgb(colors.accent))
            .child(format!("{indent}● "));
    }
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && trimmed.as_bytes().get(digits) == Some(&b'.') {
        return styled_span(
            &format!("{indent}{}. ", &trimmed[..digits]),
            colors.accent,
            FontWeight(400.),
            StyleKind::Plain,
            colors.code,
        );
    }
    if trimmed.starts_with('>') {
        return styled_span(
            "│ ",
            colors.dim,
            FontWeight(400.),
            StyleKind::Plain,
            colors.code,
        );
    }
    div()
}

fn styled_span(
    text: &str,
    color: u32,
    weight: FontWeight,
    kind: StyleKind,
    code_bg: u32,
) -> gpui::Div {
    let mut node = div().text_color(rgb(color)).font_weight(weight);
    if kind == StyleKind::Code {
        node = node.font_family("monospace").bg(rgb(code_bg)).rounded_sm();
    }
    if kind == StyleKind::Em {
        node = node.italic();
    }
    if kind == StyleKind::LinkText {
        node = node.underline();
    }
    node.child(text.to_string())
}

fn inverted_span(text: &str, weight: FontWeight, kind: StyleKind, colors: Palette) -> gpui::Div {
    let mut node = div()
        .bg(rgb(colors.caret))
        .text_color(rgb(colors.bg))
        .font_weight(weight)
        .px(px(1.));
    if kind == StyleKind::Code {
        node = node.font_family("monospace");
    }
    if kind == StyleKind::Em {
        node = node.italic();
    }
    if kind == StyleKind::LinkText {
        node = node.underline();
    }
    node.child(text.to_string())
}

fn caret(colors: Palette, mode: Mode) -> gpui::Div {
    let width = if mode == Mode::Insert { 5. } else { 12. };
    div()
        .w(px(width))
        .h(px(22.))
        .flex_shrink_0()
        .bg(rgb(colors.caret))
}

fn render_doc(doc: &StyledDoc, folder: &Path, colors: Palette) -> gpui::Div {
    let mut column = div().flex().flex_col();
    let mut previous = None;
    for block in &doc.blocks {
        let mut painted = render_block(block, folder, colors);
        if previous.is_some() && !joins_previous(previous, Some(&block.body)) {
            painted = painted.mt_2();
        }
        previous = Some(&block.body);
        column = column.child(painted);
    }
    column
}

fn render_block(block: &fell_core::compose::Block, folder: &Path, colors: Palette) -> gpui::Div {
    match &block.body {
        BlockBody::Heading { level, inlines } => div()
            .font_weight(FontWeight(680.))
            .text_size(px(if *level <= 1 { 28. } else { 22. }))
            .child(render_inlines(inlines, colors)),
        BlockBody::Paragraph { inlines } => div().child(render_inlines(inlines, colors)),
        BlockBody::List { ordered, items } => {
            let mut list = div().flex().flex_col();
            for (index, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    format!("{}.", index + 1)
                } else {
                    "●".into()
                };
                list = list.child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(18.))
                                .font_weight(FontWeight(700.))
                                .text_color(rgb(colors.accent))
                                .child(marker),
                        )
                        .child(render_inlines(item, colors)),
                );
            }
            list
        }
        BlockBody::Quote { blocks } => {
            let mut quote = div()
                .border_l(px(2.))
                .border_color(rgb(colors.dim))
                .pl_3()
                .text_color(rgb(colors.dim));
            for block in blocks {
                quote = quote.child(render_block(block, folder, colors));
            }
            quote
        }
        BlockBody::Code { text } => div()
            .font_family("monospace")
            .bg(rgb(colors.panel))
            .p_2()
            .child(text.clone()),
        BlockBody::Image { alt, src } => image_block(alt, src, folder, colors),
    }
}

fn render_inlines(inlines: &[fell_core::compose::Inline], colors: Palette) -> gpui::Div {
    let mut row = div().flex().flex_row().flex_wrap();
    for inline in inlines {
        row = match inline {
            fell_core::compose::Inline::Text(text) => row.child(div().child(text.clone())),
            fell_core::compose::Inline::Strong(inner) => row.child(
                div()
                    .font_weight(FontWeight(700.))
                    .font_family("Liberation Sans")
                    .child(render_inlines(inner, colors)),
            ),
            fell_core::compose::Inline::Em(inner) => {
                row.child(div().italic().child(render_inlines(inner, colors)))
            }
            fell_core::compose::Inline::Code(text) => row.child(
                div()
                    .font_family("monospace")
                    .bg(rgb(colors.code))
                    .px_1()
                    .rounded_sm()
                    .child(text.clone()),
            ),
            fell_core::compose::Inline::Link { text, href } => row
                .child(
                    div()
                        .text_color(rgb(colors.accent))
                        .underline()
                        .child(text.clone()),
                )
                .children(link_target(text, href).map(|target| {
                    div()
                        .pl(px(4.))
                        .text_color(rgb(colors.marker))
                        .child(target)
                })),
            fell_core::compose::Inline::Image { alt, .. } => row.child(div().child(alt.clone())),
        };
    }
    row
}

fn image_block(alt: &str, src: &str, folder: &Path, colors: Palette) -> gpui::Div {
    let block = div().flex().flex_col().items_start().gap_1();
    let Some(path) = fell_core::compose::image_file(folder, src) else {
        return missing_image(block, alt, colors);
    };
    let Some(preview) = preview_file(&path) else {
        return missing_image(block, alt, colors);
    };
    block.child(
        div().pb(px(16.)).child(
            img(preview.image)
                .w(px(preview.width))
                .h(px(preview.height))
                .object_fit(gpui::ObjectFit::Contain),
        ),
    )
}

fn missing_image(block: gpui::Div, alt: &str, colors: Palette) -> gpui::Div {
    block.child(
        div()
            .text_size(px(16.))
            .text_color(rgb(colors.dim))
            .child(alt.to_string()),
    )
}

struct Preview {
    image: Arc<RenderImage>,
    width: f32,
    height: f32,
}

const MAX_FILE_BYTES: u64 = 4_000_000;
const MAX_DECODE_EDGE: u32 = 8_192;
const MAX_DECODE_ALLOC: u64 = 64 * 1024 * 1024;
const DISPLAY_WIDTH: u32 = 420;
const MAX_CACHE_ENTRIES: usize = 8;
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;

enum Slot {
    Ready {
        stamp: SystemTime,
        len: u64,
        used: u64,
        bytes: usize,
        image: Arc<RenderImage>,
        width: f32,
        height: f32,
    },
    Rejected {
        stamp: SystemTime,
        len: u64,
        used: u64,
    },
}

impl Slot {
    fn used(&self) -> u64 {
        match self {
            Slot::Ready { used, .. } | Slot::Rejected { used, .. } => *used,
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Slot::Ready { bytes, .. } => *bytes,
            Slot::Rejected { .. } => 0,
        }
    }
}

struct PreviewCache {
    entries: HashMap<PathBuf, Slot>,
    clock: u64,
}

fn preview_cache() -> &'static Mutex<PreviewCache> {
    static CACHE: std::sync::OnceLock<Mutex<PreviewCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(PreviewCache {
            entries: HashMap::new(),
            clock: 0,
        })
    })
}

/// Drops previews for a draft that is no longer open.
fn release_previews(folder: &Path) {
    let Ok(mut cache) = preview_cache().lock() else {
        return;
    };
    cache.entries.retain(|path, _| !path.starts_with(folder));
}

fn preview_file(path: &Path) -> Option<Preview> {
    let mut cache = preview_cache().lock().ok()?;
    cache.entries.retain(|cached, _| cached.is_file());
    let meta = std::fs::metadata(path).ok()?;
    let stamp = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let len = meta.len();
    if !meta.is_file() || len > MAX_FILE_BYTES {
        remember(&mut cache, path, stamp, len, None);
        return None;
    }
    if let Some(hit) = cache.entries.get(path) {
        let same = match hit {
            Slot::Ready {
                stamp: cached_stamp,
                len: cached_len,
                ..
            }
            | Slot::Rejected {
                stamp: cached_stamp,
                len: cached_len,
                ..
            } => *cached_stamp == stamp && *cached_len == len,
        };
        if same {
            return match hit {
                Slot::Ready {
                    image,
                    width,
                    height,
                    ..
                } => Some(Preview {
                    image: Arc::clone(image),
                    width: *width,
                    height: *height,
                }),
                Slot::Rejected { .. } => None,
            };
        }
    }
    drop(cache);
    let bytes = std::fs::read(path).ok()?;
    let preview = preview_bytes(&bytes);
    let mut cache = preview_cache().lock().ok()?;
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file()
        || meta.len() != len
        || meta.modified().unwrap_or(SystemTime::UNIX_EPOCH) != stamp
    {
        return preview;
    }
    remember(&mut cache, path, stamp, len, preview.as_ref());
    preview
}

fn remember(
    cache: &mut PreviewCache,
    path: &Path,
    stamp: SystemTime,
    len: u64,
    preview: Option<&Preview>,
) {
    cache.clock = cache.clock.wrapping_add(1);
    let used = cache.clock;
    let slot = if let Some(preview) = preview {
        let bytes = (preview.width * preview.height * 4.) as usize;
        if bytes > MAX_CACHE_BYTES {
            cache.entries.remove(path);
            return;
        }
        Slot::Ready {
            stamp,
            len,
            used,
            bytes,
            image: Arc::clone(&preview.image),
            width: preview.width,
            height: preview.height,
        }
    } else {
        Slot::Rejected { stamp, len, used }
    };
    cache.entries.insert(path.to_path_buf(), slot);
    while cache.entries.len() > MAX_CACHE_ENTRIES
        || cache.entries.values().map(Slot::bytes).sum::<usize>() > MAX_CACHE_BYTES
    {
        let Some(oldest) = cache
            .entries
            .iter()
            .filter(|(cached, _)| *cached != path)
            .min_by_key(|(_, slot)| slot.used())
            .map(|(cached, _)| cached.clone())
        else {
            break;
        };
        cache.entries.remove(&oldest);
    }
}

fn preview_bytes(bytes: &[u8]) -> Option<Preview> {
    if bytes.is_empty() || u64::try_from(bytes.len()).ok()? > MAX_FILE_BYTES {
        return None;
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_EDGE);
    limits.max_image_height = Some(MAX_DECODE_EDGE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let (width, height) = fitted(image.width(), image.height())?;
    let image = if image.width() == width && image.height() == height {
        image
    } else {
        image.resize(width, height, image::imageops::FilterType::Triangle)
    };
    Some(Preview {
        image: render_rgba(image.into_rgba8()),
        width: width as f32,
        height: height as f32,
    })
}

fn fitted(width: u32, height: u32) -> Option<(u32, u32)> {
    if width == 0 || height == 0 {
        return None;
    }
    if width <= DISPLAY_WIDTH {
        return Some((width, height));
    }
    let height = u32::try_from(u64::from(height) * u64::from(DISPLAY_WIDTH) / u64::from(width))
        .ok()?
        .max(1);
    Some((DISPLAY_WIDTH, height))
}

fn render_rgba(mut data: image::RgbaImage) -> Arc<RenderImage> {
    for pixel in data.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let frame = Frame::new(data);
    let frames = SmallVec::<[Frame; 1]>::from_vec(vec![frame]);
    Arc::new(RenderImage::new(frames))
}

fn attachment_row(snap: &Snapshot, colors: Palette) -> gpui::Div {
    let names = if snap.attachments.is_empty() {
        "none".to_string()
    } else {
        snap.attachments
            .iter()
            .map(|file| {
                if file.inline {
                    format!("{} inline", file.name)
                } else {
                    file.name.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("  ·  ")
    };
    div()
        .px_5()
        .py_2()
        .text_size(px(16.))
        .bg(rgb(colors.panel))
        .text_color(rgb(colors.fg))
        .border_t_1()
        .border_color(rgb(colors.rule))
        .child(format!("Attachments  {names}"))
}

fn status_bar(
    mode: &str,
    send: &str,
    status: &SharedString,
    snap: &Snapshot,
    colors: Palette,
    wide: bool,
) -> gpui::Div {
    let fresh = snap
        .to
        .iter()
        .chain(&snap.cc)
        .chain(&snap.bcc)
        .filter(|item| item.fresh)
        .count();
    let fresh_label = match (fresh, wide) {
        (0, _) => None,
        (1, true) => Some("1 new recipient".to_string()),
        (1, false) => Some("1 new".to_string()),
        (count, true) => Some(format!("{count} new recipients")),
        (count, false) => Some(format!("{count} new")),
    };
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_4()
        .px(px(16.))
        .py_2()
        .bg(rgb(colors.panel))
        .border_t_1()
        .border_color(rgb(colors.rule))
        .text_size(px(16.))
        .child(
            div()
                .flex()
                .flex_shrink_0()
                .gap_3()
                .child(div().text_color(rgb(colors.accent)).child(mode.to_string()))
                .child(div().text_color(rgb(colors.fg)).child(status.to_string()))
                .children(
                    fresh_label.map(|label| div().text_color(rgb(colors.accent)).child(label)),
                ),
        )
        .child(div().flex_shrink_0().child(send.to_string()))
}

fn which_key(colors: Palette) -> gpui::Div {
    div()
        .absolute()
        .bottom(px(48.))
        .left(px(24.))
        .bg(rgb(colors.panel))
        .border_1()
        .border_color(rgb(colors.selection))
        .p_3()
        .text_sm()
        .child("space m  mail")
        .child(
            div()
                .text_color(rgb(colors.dim))
                .child("s send   p preview   r raw   q quote   t theme"),
        )
        .child("space a  agent edit")
        .child(div().text_color(rgb(colors.dim)).child("k keep   x reject"))
}

fn palette_view(query: &str, colors: Palette) -> gpui::Div {
    div()
        .absolute()
        .top(px(80.))
        .left(px(80.))
        .right(px(80.))
        .bg(rgb(colors.panel))
        .border_1()
        .border_color(rgb(colors.accent))
        .p_3()
        .child(format!(":{query}"))
        .child(
            div()
                .text_sm()
                .text_color(rgb(colors.dim))
                .child("send · preview · raw · quote · theme · keep · reject · undo"),
        )
}

fn preview_layer(snap: &Snapshot, colors: Palette) -> impl IntoElement {
    let doc = markdown_to_doc(&snap.body);
    div()
        .absolute()
        .top(px(64.))
        .bottom(px(64.))
        .left(px(48.))
        .right(px(48.))
        .bg(rgb(colors.bg))
        .border_1()
        .border_color(rgb(colors.accent))
        .id("html-preview")
        .p_4()
        .overflow_y_scroll()
        .child(
            div()
                .text_sm()
                .text_color(rgb(colors.dim))
                .child("HTML preview"),
        )
        .child(render_doc(&doc, Path::new(""), colors))
}

fn join_addrs(addresses: &[fell_core::compose::AddressView]) -> String {
    addresses
        .iter()
        .map(|address| address.raw.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

fn empty_snapshot(id: &str) -> Snapshot {
    Snapshot {
        id: id.to_string(),
        revision: 0,
        from: String::new(),
        to: Vec::new(),
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: String::new(),
        body: String::new(),
        in_reply_to: None,
        hunks: Vec::new(),
        issues: Vec::new(),
        notices: Vec::new(),
        attachments: Vec::new(),
        unreviewed: 0,
        quoted: None,
        open: false,
        focused: false,
        send_block: None,
        text: String::new(),
    }
}

/// Stores the editor text. One insert burst is one undo step: the group the
/// first keystroke opened stays open until the insert ends.
fn record_body(session: &mut fell_core::compose::DraftSession, body: String, mode: Mode) {
    session.set_body_from_user(body);
    if mode != Mode::Insert {
        session.end_user_change();
    }
}

fn stored_quote(snap: &Snapshot) -> Option<&str> {
    snap.quoted
        .as_deref()
        .filter(|html| !html.trim().is_empty())
}

/// The destination shown after a link label, so a changed URL is visible
/// before send. Omitted when the label already is the URL.
fn link_target(text: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href == text.trim() {
        return None;
    }
    Some(format!("({href})"))
}

fn canonical_body(body: &str) -> String {
    if body.is_empty() || body == "\n" {
        return "\n".to_string();
    }
    if body.ends_with('\n') {
        body.to_string()
    } else {
        format!("{body}\n")
    }
}

fn byte_index(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map(|(index, _)| index)
        .unwrap_or(text.len())
}

fn cursor_byte(text: &str, row: usize, col: usize) -> usize {
    let mut byte = 0;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        if index == row {
            let without = line.trim_end_matches('\n');
            return byte + byte_index(without, col);
        }
        byte += line.len();
    }
    text.len()
}

fn palette_action(query: &str) -> Option<&'static str> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return None;
    }
    let mut found = None;
    for name in [
        "send", "preview", "raw", "quote", "theme", "keep", "reject", "undo",
    ] {
        if name.starts_with(&query) {
            if found.is_some() {
                return None;
            }
            found = Some(name);
        }
    }
    found
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn split_at_char_boundary(text: &str, at: usize) -> (&str, &str) {
    let at = at.min(text.len());
    if text.is_char_boundary(at) {
        text.split_at(at)
    } else {
        (text, "")
    }
}

pub fn demo_body() -> &'static str {
    "# Q3 numbers\n\nThe quarter closed **ahead** of plan. See the [board note](https://example.com/q3).\n\nShip the `q3` figure, and keep it in *draft*.\n\n- Revenue up\n- Margin held\n\n> A quote from the last review\n\n![chart](chart.png)\n"
}

/// The demo draft's quoted part. Real drafts paint what the session stores.
fn demo_quote() -> &'static str {
    "<p>Earlier note on the quarter.</p><ul><li>Revenue</li></ul><blockquote><p>Keep the margin.</p></blockquote>"
}

pub fn open_demo(
    root: PathBuf,
    theme: ThemeKind,
) -> Result<(Arc<DraftHub>, String, PathBuf), String> {
    let fields = fell_core::compose::DraftFields {
        from: "jon@example.com".into(),
        to: vec!["Ada <ada@example.com>".into()],
        cc: vec![],
        bcc: vec!["audit@example.com".into()],
        subject: "Re: Q3 numbers".into(),
        in_reply_to: Some("<q3@example.com>".into()),
        attachments: vec![],
    };
    let folder = fell_core::compose::materialize_in(
        &root,
        "demo",
        &fields,
        demo_body(),
        &AccountId("local".into()),
    )?;
    std::fs::write(folder.join("chart.png"), crate::png::chart_png())
        .map_err(|err| err.to_string())?;
    let mut session = fell_core::compose::DraftSession::create(
        folder.clone(),
        "demo",
        AccountId("local".into()),
        fields,
        demo_body(),
    )?;
    session.open_for_edit();
    session.set_known_recipients(["jon@example.com".into()].into_iter().collect());
    session.set_quote(Some(demo_quote().into()));
    let hub = Arc::new(DraftHub::new());
    hub.insert(session);
    let _ = theme;
    Ok((hub, "demo".into(), folder))
}

#[cfg(test)]
mod tests {
    use super::cursor_line_covered;
    use fell_core::compose::markdown_to_doc;

    fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc_bytes = kind.to_vec();
        crc_bytes.extend_from_slice(data);
        let mut crc = 0xffff_ffffu32;
        for byte in &crc_bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = 0u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        out.extend((!crc).to_be_bytes());
    }

    #[test]
    fn an_attached_png_decodes_to_its_own_pixels() {
        let preview = super::preview_bytes(&crate::png::chart_png()).expect("chart");
        assert!((preview.width - 240.).abs() < 0.1);
        assert!((preview.height - 105.).abs() < 0.1);
        let image = image::load_from_memory(&crate::png::chart_png())
            .unwrap()
            .to_rgba8();
        let tall = image.get_pixel(90, 20).0;
        let short = image.get_pixel(20, 20).0;
        assert_ne!(tall, short);
    }

    #[test]
    fn the_tallest_demo_bar_is_inset_from_the_top_edge() {
        let image = image::load_from_memory(&crate::png::chart_png())
            .unwrap()
            .to_rgba8();
        let background = image.get_pixel(0, 0).0;
        for y in 0..12 {
            assert_eq!(image.get_pixel(160, y).0, background, "row {y}");
        }
        assert_ne!(image.get_pixel(160, 12).0, background);
        let bottom = image.height() - 1;
        assert_eq!(image.get_pixel(160, bottom).0, background);
    }

    #[test]
    fn a_wide_photo_is_downscaled_instead_of_discarded() {
        let image = image::RgbaImage::from_pixel(3000, 100, image::Rgba([9, 8, 7, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        let preview = super::preview_bytes(&bytes).expect("wide");
        assert!((preview.width - 420.).abs() < 0.1);
        assert!((preview.height - 14.).abs() < 0.1);
    }

    #[test]
    fn an_image_past_the_decoder_edge_is_refused() {
        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        let mut ihdr = Vec::new();
        ihdr.extend(20_000u32.to_be_bytes());
        ihdr.extend(20_000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        png_chunk(&mut png, b"IHDR", &ihdr);
        png_chunk(&mut png, b"IEND", &[]);
        assert!(super::preview_bytes(&png).is_none());
    }

    #[test]
    fn preview_cache_rejects_without_rereading_and_drops() {
        let root = std::env::temp_dir().join(format!(
            "fell-preview-{}-{}",
            std::process::id(),
            super::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert!(super::preview_file(&root).is_none());

        let huge = root.join("huge.bin");
        let file = std::fs::File::create(&huge).unwrap();
        file.set_len(super::MAX_FILE_BYTES + 1).unwrap();
        drop(file);
        assert!(super::preview_file(&huge).is_none());
        assert!(super::preview_file(&huge).is_none());

        let bad = root.join("bad.png");
        let chart = crate::png::chart_png();
        std::fs::write(&bad, vec![0u8; chart.len()]).unwrap();
        let stamp = std::fs::metadata(&bad).unwrap().modified().unwrap();
        assert!(super::preview_file(&bad).is_none());
        std::fs::write(&bad, &chart).unwrap();
        std::fs::File::open(&bad)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
        assert!(
            super::preview_file(&bad).is_none(),
            "a rejection sticks until size or mtime changes"
        );
        let later = stamp + std::time::Duration::from_secs(2);
        std::fs::File::open(&bad)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert!(super::preview_file(&bad).is_some());

        std::fs::remove_file(&bad).unwrap();
        let other = root.join("other.png");
        std::fs::write(&other, &chart).unwrap();
        assert!(super::preview_file(&other).is_some());
        let cache = super::preview_cache().lock().unwrap();
        assert!(!cache.entries.contains_key(&bad));
        drop(cache);

        for index in 0..super::MAX_CACHE_ENTRIES + 3 {
            let path = root.join(format!("n{index}.png"));
            std::fs::write(&path, &chart).unwrap();
            assert!(super::preview_file(&path).is_some());
        }
        let cache = super::preview_cache().lock().unwrap();
        let kept = cache
            .entries
            .keys()
            .filter(|path| path.starts_with(&root))
            .count();
        assert!(kept <= super::MAX_CACHE_ENTRIES);
        drop(cache);

        super::release_previews(&root);
        let cache = super::preview_cache().lock().unwrap();
        assert!(cache.entries.keys().all(|path| !path.starts_with(&root)));
        drop(cache);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_substring_of_send_is_not_a_send_command() {
        assert_eq!(super::palette_action("d"), None);
        assert_eq!(super::palette_action("e"), None);
        assert_eq!(super::palette_action("n"), None);
        assert_eq!(super::palette_action("end"), None);
        assert_eq!(super::palette_action(""), None);
        assert_eq!(super::palette_action("s"), Some("send"));
        assert_eq!(super::palette_action("send"), Some("send"));
        assert_eq!(super::palette_action("se"), Some("send"));
        assert_eq!(super::palette_action("r"), None);
        assert_eq!(super::palette_action("re"), Some("reject"));
        assert_eq!(super::palette_action("raw"), Some("raw"));
    }

    #[test]
    fn empty_and_trailing_blank_lines_match_the_session() {
        assert_eq!(super::canonical_body(""), super::canonical_body("\n"));
        let empty = super::Editor::from_body("");
        assert_eq!(
            super::canonical_body(&empty.text()),
            super::canonical_body("")
        );
        let blank = super::Editor::from_body("hello\n\n");
        assert_eq!(blank.text(), "hello\n\n");
        assert_eq!(
            super::canonical_body(&blank.text()),
            super::canonical_body("hello\n\n")
        );
    }

    #[test]
    fn a_blank_cursor_line_is_not_inside_a_block() {
        let text = "# Q3\n\nNext\n";
        let doc = markdown_to_doc(text);
        assert!(cursor_line_covered(text, 0, &doc));
        assert!(!cursor_line_covered(text, 1, &doc));
        assert!(cursor_line_covered(text, 2, &doc));
        let blank = markdown_to_doc("\n");
        assert!(!cursor_line_covered("\n", 0, &blank));
    }

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "fell-compose-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn plain_session(tag: &str, body: &str) -> fell_core::compose::DraftSession {
        let fields = fell_core::compose::DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            subject: "Hello".into(),
            ..fell_core::compose::DraftFields::default()
        };
        let account = fell_core::AccountId("local".into());
        let folder =
            fell_core::compose::materialize_in(&temp_root(tag), tag, &fields, body, &account)
                .unwrap();
        fell_core::compose::DraftSession::create(folder, tag, account, fields, body).unwrap()
    }

    #[test]
    fn one_undo_takes_back_a_whole_insert_burst() {
        let before = "Ship it.\n";
        let mut session = plain_session("undo", before);
        let mut editor = super::Editor::from_body(before);
        editor.row = 0;
        editor.col = editor.lines[0].chars().count();
        editor.mode = super::Mode::Insert;
        for ch in "hello".chars() {
            editor.lines[0].push(ch);
            editor.col += 1;
            super::record_body(&mut session, editor.text(), editor.mode);
        }
        assert_eq!(session.body(), "Ship it.hello\n");
        // Escape ends the insert.
        editor.mode = super::Mode::Normal;
        session.end_user_change();
        assert!(session.undo());
        assert_eq!(session.body(), before);
        assert!(!session.undo(), "the burst was a single step");
    }

    #[test]
    fn a_normal_mode_edit_is_its_own_undo_step() {
        let mut session = plain_session("undo-normal", "abc\n");
        super::record_body(&mut session, "bc\n".into(), super::Mode::Normal);
        super::record_body(&mut session, "c\n".into(), super::Mode::Normal);
        assert!(session.undo());
        assert_eq!(session.body(), "bc\n");
    }

    #[test]
    fn a_block_range_end_belongs_to_the_next_line() {
        use super::block_holds_cursor;
        // "# Q3\n" spans 0..5; byte 5 starts the next line.
        assert!(block_holds_cursor((0, 5), 0, 12));
        assert!(block_holds_cursor((0, 5), 4, 12));
        assert!(!block_holds_cursor((0, 5), 5, 12));
        assert!(!block_holds_cursor((6, 11), 5, 12));
        // At the end of the buffer there is no next line to take the byte.
        assert!(block_holds_cursor((0, 5), 5, 5));
    }

    #[test]
    fn the_block_above_a_blank_cursor_line_keeps_its_style() {
        use super::BodyItem::{Block, CaretRow};
        let text = "# Q3\n\nNext\n";
        let doc = markdown_to_doc(text);
        assert_eq!(
            super::body_layout(text, 1, 0, &doc),
            vec![
                Block {
                    index: 0,
                    raw: false
                },
                CaretRow,
                Block {
                    index: 1,
                    raw: false
                },
            ]
        );
        assert_eq!(
            super::body_layout(text, 0, 4, &doc),
            vec![
                Block {
                    index: 0,
                    raw: true
                },
                Block {
                    index: 1,
                    raw: false
                },
            ]
        );
    }

    #[test]
    fn the_first_byte_of_the_next_block_does_not_unstyle_the_one_above() {
        use super::BodyItem::Block;
        let text = "# Heading\nParagraph\n";
        let doc = markdown_to_doc(text);
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(
            super::body_layout(text, 1, 0, &doc),
            vec![
                Block {
                    index: 0,
                    raw: false
                },
                Block {
                    index: 1,
                    raw: true
                },
            ]
        );
    }

    #[test]
    fn trailing_and_empty_blank_lines_get_one_caret_row() {
        use super::BodyItem::{Block, CaretRow};
        let text = "hello\n\n";
        let doc = markdown_to_doc(text);
        assert_eq!(
            super::body_layout(text, 1, 0, &doc),
            vec![
                Block {
                    index: 0,
                    raw: false
                },
                CaretRow
            ]
        );
        let empty = markdown_to_doc("\n");
        assert_eq!(super::body_layout("\n", 0, 0, &empty), vec![CaretRow]);
    }

    #[test]
    fn an_image_under_a_quote_has_no_block_gap() {
        let doc = markdown_to_doc("> A quote\n\n![chart](chart.png)\n\nAfter\n");
        let [quote, image, after] = doc.blocks.as_slice() else {
            panic!("three blocks");
        };
        assert!(super::joins_previous(Some(&quote.body), Some(&image.body)));
        assert!(!super::joins_previous(Some(&image.body), Some(&after.body)));
        assert!(!super::joins_previous(Some(&quote.body), Some(&after.body)));
        assert!(!super::joins_previous(Some(&quote.body), None));
        assert!(!super::joins_previous(None, Some(&image.body)));
    }

    #[test]
    fn a_link_shows_where_it_goes() {
        assert_eq!(
            super::link_target("board note", "https://example.com/q3").as_deref(),
            Some("(https://example.com/q3)")
        );
        assert_eq!(
            super::link_target("https://example.com", "https://example.com"),
            None
        );
        assert_eq!(super::link_target("empty", "  "), None);
    }

    #[test]
    fn the_quoted_original_is_the_stored_quote() {
        let mut session = plain_session("quote", "body\n");
        assert_eq!(super::stored_quote(&session.snapshot()), None);
        session.set_quote(Some("  \n".into()));
        assert_eq!(super::stored_quote(&session.snapshot()), None);
        session.set_quote(Some("<p>Original</p>".into()));
        assert_eq!(
            super::stored_quote(&session.snapshot()),
            Some("<p>Original</p>")
        );

        let (hub, id, _) =
            super::open_demo(temp_root("demo-quote"), super::ThemeKind::Dark).expect("demo draft");
        let demo = hub.with_session(&id, |session| session.snapshot()).unwrap();
        assert_eq!(super::stored_quote(&demo), Some(super::demo_quote()));
    }
}
