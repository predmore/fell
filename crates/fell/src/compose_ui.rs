//! Compose window. The buffer is plain markdown. Styling is a view of that
//! text, through the same document model the conversation renderer uses.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fell_core::compose::{
    BlockBody, DraftHub, HunkKind, Snapshot, StyleKind, StyledDoc, html_to_doc, line_styles,
    markdown_to_doc,
};
use fell_core::{AccountId, MailBackend};
use gpui::{
    Context, FocusHandle, FontWeight, KeyDownEvent, ObjectFit, SharedString,
    StatefulInteractiveElement, Window, div, img, prelude::*, px, rgb,
};

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
    hunk: u32,
}

fn palette(kind: ThemeKind) -> Palette {
    match kind {
        ThemeKind::Dark => Palette {
            bg: 0x1c1b19,
            fg: 0xf4f0ea,
            dim: 0xe7e2db,
            marker: 0xe8e4dc,
            accent: 0x7aa2f7,
            caret: 0xffffff,
            code: 0x3a3834,
            selection: 0x2a3344,
            error: 0xf7768e,
            fresh: 0x3a2e22,
            panel: 0x242320,
            hunk: 0x243044,
        },
        ThemeKind::Light => Palette {
            bg: 0xf7f5f2,
            fg: 0x1c1b19,
            dim: 0x3a3632,
            marker: 0x2c2926,
            accent: 0x1d4f91,
            caret: 0x111111,
            code: 0xe4ddd4,
            selection: 0xd9e2f2,
            error: 0x9f1239,
            fresh: 0xe4c9a4,
            panel: 0xfffdfb,
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
        let mut lines: Vec<String> = body
            .trim_end_matches('\n')
            .split('\n')
            .map(str::to_string)
            .collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        let row = lines
            .iter()
            .position(|line| line.starts_with("- Revenue"))
            .unwrap_or(0);
        let col = lines
            .get(row)
            .filter(|line| line.starts_with("- ") || line.starts_with("* "))
            .map(|_| 2)
            .unwrap_or(0);
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
        if !text.ends_with('\n') {
            text.push('\n');
        }
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
    watching: bool,
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
            watching: false,
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
                this.status = "esc normal".into();
                cx.notify();
            });
            cx.background_executor()
                .timer(Duration::from_millis(800))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.editor.mode = Mode::Normal;
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
        let Some(snap) = self
            .hub
            .with_session(self.draft_id.as_ref(), |session| session.snapshot())
        else {
            return;
        };
        if snap.revision != self.applied_revision && self.field == Field::Body {
            let incoming = snap.body.clone();
            if incoming != self.editor.text() {
                let row = self.editor.row;
                let col = self.editor.col;
                self.editor = Editor::from_body(&incoming);
                self.editor.row = row;
                self.editor.col = col;
                self.editor.mode = Mode::Normal;
                self.editor.clamp();
            }
            self.to_line = join_addrs(&snap.to);
            self.cc_line = join_addrs(&snap.cc);
            self.bcc_line = join_addrs(&snap.bcc);
            self.subject_line = snap.subject.clone();
            self.applied_revision = snap.revision;
        }
    }

    fn push_body(&mut self) {
        let body = self.editor.text();
        let id = self.draft_id.to_string();
        if let Some(revision) = self.hub.with_session(&id, |session| {
            session.set_body_from_user(body);
            session.end_user_change();
            let _ = session.flush();
            session.revision()
        }) {
            self.applied_revision = revision;
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
                if let Some(revision) = self.hub.with_session(&id, |session| {
                    let _ = session.flush();
                    session.revision()
                }) {
                    self.applied_revision = revision;
                }
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

    fn send(&mut self) {
        if self.field != Field::Body && !self.commit_field(self.field) {
            return;
        }
        let id = self.draft_id.to_string();
        let account = self
            .hub
            .with_session(&id, |session| session.account_id().clone());
        let Some(account) = account else {
            self.status = "draft is not open".into();
            return;
        };
        let backend = fell_core::backend_for(&account);
        let outcome = self.hub.with_session(&id, |session| {
            session.mark_seen();
            send_from_editor(session, backend.as_ref())
        });
        match outcome {
            Some(Ok(_)) => {
                self.sent = true;
                self.status = "Sent from the editor".into();
            }
            Some(Err(err)) => self.status = err.into(),
            None => self.status = "draft is not open".into(),
        }
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
            self.palette_key(key, typed);
            return;
        }
        if self.leader {
            self.leader_key(key);
            return;
        }
        if key == "escape" {
            if self.field != Field::Body {
                let field = self.field;
                if !self.commit_field(field) {
                    self.sync_from_hub();
                }
            }
            self.editor.mode = Mode::Normal;
            self.editor.anchor = None;
            self.field = Field::Body;
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
    }

    fn palette_key(&mut self, key: &str, typed: Option<&str>) {
        let Some(query) = self.palette.as_mut() else {
            return;
        };
        match key {
            "escape" => self.palette = None,
            "enter" => {
                let query = query.clone();
                self.palette = None;
                self.run_palette(&query);
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

    fn run_palette(&mut self, query: &str) {
        let query = query.trim().to_ascii_lowercase();
        let action = [
            ("send", "send"),
            ("preview", "preview"),
            ("raw", "raw"),
            ("quote", "quote"),
            ("theme", "theme"),
            ("keep", "keep"),
            ("reject", "reject"),
            ("undo", "undo"),
        ]
        .into_iter()
        .find(|(name, _)| name.contains(&query) || query.is_empty() && *name == "send");
        if let Some((_, action)) = action {
            self.run_action(action);
        }
    }

    fn run_action(&mut self, action: &str) {
        match action {
            "send" => self.send(),
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
            "quote" => self.quote_open = !self.quote_open,
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

    fn leader_key(&mut self, key: &str) {
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
            self.run_action(action);
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
        let _ = self.hub.with_session(self.draft_id.as_ref(), |session| {
            session.mark_seen();
            session.open_for_edit();
        });
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
        let send_label = if let Some(reason) = &snap.send_block {
            format!("send blocked · {reason}")
        } else if unreviewed == 0 {
            "send  space m s".to_string()
        } else {
            format!("send  space m s  ·  {unreviewed} unreviewed")
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
            .font_family("Liberation Sans")
            .child(header(&snap, colors).flex_shrink_0())
            .child(self.fields_view(&snap, colors).flex_shrink_0())
            .child(self.body_view(&snap, colors))
            .child(self.quote_view(colors).flex_shrink_0())
            .child(attachment_row(&snap, colors).flex_shrink_0())
            .child(status_bar(mode, &send_label, &hint, &snap, colors).flex_shrink_0())
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
            .child(reply_row(snap, colors))
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
        if !snap.hunks.is_empty() {
            column = column.child(hunk_banner(snap, colors));
        }
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

    fn quote_view(&self, colors: Palette) -> gpui::Div {
        let label = if self.quote_open {
            "Quoted original"
        } else {
            "Quoted original  ·  space m q"
        };
        let mut block = div()
            .px_5()
            .py_2()
            .text_size(px(16.))
            .text_color(rgb(colors.dim))
            .border_t_1()
            .border_color(rgb(colors.selection))
            .child(label);
        if self.quote_open {
            let doc = html_to_doc(
                "<p>Earlier note on the quarter.</p><ul><li>Revenue</li></ul><blockquote><p>Keep the margin.</p></blockquote>",
            );
            block = block.child(render_doc(&doc, &self.folder, colors));
        }
        block
    }
}

pub fn send_from_editor(
    session: &mut fell_core::compose::DraftSession,
    backend: &dyn MailBackend,
) -> Result<fell_core::SentMessage, String> {
    session.mark_seen();
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
    let pending = backend
        .submit_composed(outgoing.draft, outgoing.mime, files)
        .map_err(|err| err.to_string())?;
    let confirm = confirmation_from_ui(&pending);
    backend
        .confirm_send(pending, confirm)
        .map_err(|err| err.to_string())
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
            .border_color(rgb(if active { colors.accent } else { bg }))
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
        .px_5()
        .py_2()
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
    let cursor_byte = cursor_byte(&text, editor.row, editor.col);
    let mut column = div().flex().flex_col().gap_2();
    if doc.blocks.is_empty() {
        column = column.child(div().text_color(rgb(colors.dim)).child("Write in markdown"));
    }
    for block in &doc.blocks {
        let on_cursor = block
            .source
            .is_some_and(|(start, end)| cursor_byte >= start && cursor_byte <= end);
        let edited = block.source.is_some_and(|(start, end)| {
            hunks
                .iter()
                .any(|(hunk_start, hunk_end)| start < *hunk_end && end > *hunk_start)
        });
        let painted = if on_cursor || block.source.is_none() && editor.row == 0 {
            raw_source_block(block, &text, editor, colors)
        } else {
            render_block(block, folder, colors)
        };
        column = column.child(if edited {
            div().bg(rgb(colors.hunk)).px_2().py_1().child(painted)
        } else {
            painted
        });
    }
    column
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
        let color = match span.kind {
            StyleKind::Marker | StyleKind::LinkUrl => colors.marker,
            StyleKind::LinkText => colors.accent,
            _ => colors.fg,
        };
        let weight = match span.kind {
            StyleKind::Strong | StyleKind::Marker => FontWeight(700.),
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
        node = node
            .font_family("monospace")
            .bg(rgb(code_bg))
            .px_1()
            .rounded_sm();
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
        .px(px(4.))
        .py(px(1.));
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
    let mut column = div().flex().flex_col().gap_2();
    for block in &doc.blocks {
        column = column.child(render_block(block, folder, colors));
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
            fell_core::compose::Inline::Link { text, .. } => row.child(
                div()
                    .text_color(rgb(colors.accent))
                    .underline()
                    .child(text.clone()),
            ),
            fell_core::compose::Inline::Image { alt, .. } => row.child(div().child(alt.clone())),
        };
    }
    row
}

fn image_block(alt: &str, src: &str, folder: &Path, colors: Palette) -> gpui::Div {
    let path = fell_core::compose::image_file(folder, src);
    let mut block = div().flex().flex_col().items_start().gap_1();
    if let Some(path) = path {
        block = block.child(
            img(path)
                .w(px(240.))
                .h(px(105.))
                .object_fit(ObjectFit::None)
                .flex_shrink_0(),
        );
    }
    block.child(
        div()
            .pb_3()
            .text_size(px(16.))
            .text_color(rgb(colors.dim))
            .child(alt.to_string()),
    )
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
        .text_color(rgb(colors.dim))
        .child(format!("Attachments  {names}"))
}

fn status_bar(
    mode: &str,
    send: &str,
    status: &SharedString,
    snap: &Snapshot,
    colors: Palette,
) -> gpui::Div {
    let fresh = snap
        .to
        .iter()
        .chain(&snap.cc)
        .chain(&snap.bcc)
        .filter(|item| item.fresh)
        .count();
    let fresh_label = match fresh {
        0 => None,
        1 => Some("1 new recipient".to_string()),
        count => Some(format!("{count} new recipients")),
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .px_4()
        .py_2()
        .bg(rgb(colors.panel))
        .border_t_1()
        .border_color(rgb(colors.selection))
        .text_size(px(16.))
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_3()
                .child(div().text_color(rgb(colors.accent)).child(mode.to_string()))
                .child(div().text_color(rgb(colors.fg)).child(status.to_string()))
                .children(
                    fresh_label.map(|label| div().text_color(rgb(colors.accent)).child(label)),
                ),
        )
        .child(div().flex().justify_end().child(send.to_string()))
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
    let folder = fell_core::compose::materialize_in(&root, "demo", &fields, demo_body())?;
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
    let hub = Arc::new(DraftHub::new());
    hub.insert(session);
    let _ = theme;
    Ok((hub, "demo".into(), folder))
}
