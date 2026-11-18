//! Compose window. The buffer is plain markdown. Styling is a view of that
//! text, through the same document model the conversation renderer uses.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use fell_core::compose::{
    BlockBody, DraftHub, HunkKind, SendLock, Snapshot, StyleKind, StyledDoc, html_to_doc,
    line_styles, markdown_to_doc,
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
    panel: u32,
    rule: u32,
    hunk: u32,
    /// The focused agent edit: a stronger tint than the others.
    hunk_focus: u32,
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
            panel: 0x242320,
            rule: 0x6e675f,
            hunk: 0x243044,
            hunk_focus: 0x2e3f5c,
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
            panel: 0xfffdfb,
            rule: 0x8f877e,
            hunk: 0xe7eef8,
            hunk_focus: 0xd3e0f2,
        },
    }
}

/// Compose colors from the Omarchy theme the inbox is using, so the editor
/// matches it. Tinted rows keep AA text on them.
fn palette_from_theme(theme: &crate::theme::Theme) -> Palette {
    use crate::theme::{MARK_CONTRAST, TEXT_CONTRAST, ensure_all, mix};
    let bg = theme.background;
    let panel = theme.surface;
    let code = mix(bg, theme.foreground, 0.10);
    let hunk = mix(bg, theme.accent, 0.16);
    let hunk_focus = mix(bg, theme.accent, 0.28);
    let fg = ensure_all(
        theme.foreground,
        &[bg, panel, code, hunk, hunk_focus],
        TEXT_CONTRAST,
    );
    Palette {
        bg,
        fg,
        dim: ensure_all(
            theme.dimmed,
            &[bg, panel, code, hunk, hunk_focus],
            TEXT_CONTRAST,
        ),
        marker: ensure_all(
            mix(theme.dimmed, bg, 0.2),
            &[bg, code, hunk, hunk_focus],
            MARK_CONTRAST,
        ),
        accent: ensure_all(theme.accent, &[bg, panel, hunk, hunk_focus], TEXT_CONTRAST),
        caret: fg,
        code,
        selection: theme.selection,
        error: ensure_all(theme.error, &[bg, panel], TEXT_CONTRAST),
        panel,
        rule: theme.border,
        hunk,
        hunk_focus,
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
    /// A header field takes the typing, so the body draws no caret.
    caret_hidden: bool,
    anchor: Option<(usize, usize)>,
    preferred: usize,
    count: usize,
}

impl Editor {
    /// After the text under the editor changed, put the cursor back on the
    /// line it was on: the same row if that row still holds `line`, else the
    /// one row that does. False when there is no such single row.
    fn follow_line(&mut self, line: &str) -> bool {
        if self.lines.get(self.row).is_some_and(|row| row == line) {
            return true;
        }
        let mut matches = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, row)| row.as_str() == line);
        match (matches.next(), matches.next()) {
            (Some((row, _)), None) => {
                self.row = row;
                self.clamp();
                true
            }
            _ => false,
        }
    }

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
            caret_hidden: false,
            anchor: None,
            preferred: 0,
            count: 0,
        }
    }

    /// The cursor after the last written text, or at the top of an empty
    /// body.
    fn cursor_to_end_of_text(&mut self) {
        match self.lines.iter().rposition(|line| !line.trim().is_empty()) {
            Some(row) => {
                self.row = row;
                self.col = self.lines[row].chars().count();
            }
            None => {
                self.row = 0;
                self.col = 0;
            }
        }
        self.preferred = self.col;
        self.clamp();
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

/// Tab or Enter on a header field: the next field. Leaving Subject, the
/// typing carries on in the body in insert mode, as it did in the field, so
/// the next keys are text and not commands.
fn leave_header(field: Field, editor: &mut Editor) -> Field {
    let next = match field {
        Field::To => Field::Cc,
        Field::Cc => Field::Bcc,
        Field::Bcc => Field::Subject,
        Field::Subject | Field::Body => Field::Body,
    };
    if next == Field::Body {
        editor.mode = Mode::Insert;
        editor.anchor = None;
        editor.clamp();
    }
    next
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
    /// The Omarchy theme this window follows, live. `None` uses the
    /// built-in palette for `theme`.
    look: Option<crate::theme::Theme>,
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
    /// The draft whose send is talking to the server. Another send of it
    /// waits. The window may have moved to another draft meanwhile.
    sending: Option<String>,
    watching: bool,
    flush_pending: bool,
    flush_gen: u64,
    /// `:q` asked to close this window. The next frame removes it.
    closing: bool,
    /// Close once a send went out, so the person is back in the inbox and
    /// the sent draft does not linger. The demo window stays, showing sent.
    close_on_send: bool,
    /// `]` or `[` was pressed; `h` then jumps to the next or previous edit.
    bracket: Option<char>,
    /// The agent edit that the bar describes and keep, reject act on.
    focused_hunk: Option<u64>,
    /// How many agent edits the window has already announced.
    seen_edits: usize,
    /// The person moved between edits; arrivals then leave focus alone.
    focus_by_user: bool,
    /// The person pressed a key in this window; until then the cursor
    /// follows the focused edit as it does on opening.
    touched: bool,
    /// Agent edits already announced, so an arrival can be named.
    known_hunks: Vec<u64>,
    /// Send was tried: a missing recipient now says it blocks send.
    send_tried: bool,
    /// The header took the keyboard: Cc and Bcc keep their own rows from
    /// then on, so tabbing through them never moves the body.
    header_open: bool,
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
        // A closed window no longer counts as showing its draft.
        cx.on_release(|this: &mut Self, _| this.hub.note_hidden(this.draft_id.as_ref()))
            .detach();
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
            look: None,
            raw: false,
            preview: false,
            quote_open: false,
            focus: cx.focus_handle(),
            leader: false,
            leader_at: None,
            which_visible: false,
            palette: None,
            status: "".into(),
            sent: false,
            sending: None,
            watching: false,
            flush_pending: false,
            flush_gen: 0,
            closing: false,
            close_on_send: false,
            bracket: None,
            focused_hunk: None,
            seen_edits: 0,
            focus_by_user: false,
            touched: false,
            known_hunks: Vec::new(),
            send_tried: false,
            header_open: false,
        }
    }

    pub fn watch_live(&self, cx: &mut Context<Self>) {
        // An async channel: waiting for the hub holds no thread, and once
        // the window is gone the next change drops this subscription.
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        self.hub
            .subscribe_with(move || tx.unbounded_send(()).is_ok());
        cx.spawn(async move |this, cx| {
            while futures::StreamExt::next(&mut rx).await.is_some() {
                // Several changes may have queued; one repaint covers them.
                while rx.try_recv().is_ok() {}
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
                    this.status = "".into();
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
                this.status = "".into();
                cx.notify();
            });
            cx.background_executor()
                .timer(Duration::from_millis(800))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.editor.mode = Mode::Normal;
                this.end_insert();
                this.status = "".into();
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

    /// Paint with this Omarchy theme, as the inbox does. The host calls it
    /// again when the theme changes. `:theme` switches to the built-ins.
    pub(crate) fn set_look(&mut self, theme: crate::theme::Theme) {
        self.theme = if theme.is_light() {
            ThemeKind::Light
        } else {
            ThemeKind::Dark
        };
        self.look = Some(theme);
    }

    /// Whether this window still follows the Omarchy theme.
    pub(crate) fn follows_look(&self) -> bool {
        self.look.is_some()
    }

    /// Move to the next (or previous) agent edit, wrapping around, and put
    /// the cursor on it when it is in the body.
    fn goto_hunk(&mut self, forward: bool) {
        self.focus_by_user = true;
        let id = self.draft_id.to_string();
        let Some(snap) = self.hub.with_session(&id, |session| session.snapshot()) else {
            return;
        };
        let order = ordered_hunks(&snap);
        if order.is_empty() {
            self.focused_hunk = None;
            self.status = "no agent edits".into();
            return;
        }
        let at = self
            .focused_hunk
            .and_then(|focused| order.iter().position(|(hunk, _)| *hunk == focused));
        let next = match (at, forward) {
            (None, _) => 0,
            (Some(at), true) => (at + 1) % order.len(),
            (Some(at), false) => (at + order.len() - 1) % order.len(),
        };
        self.focus_hunk_at(&order, next);
    }

    fn focus_hunk_at(&mut self, order: &[(u64, Option<usize>)], index: usize) {
        let Some((hunk, line)) = order.get(index) else {
            self.focused_hunk = None;
            return;
        };
        self.focused_hunk = Some(*hunk);
        if let Some(line) = line {
            self.editor.row = line - 1;
            // Past a list or quote marker, so the cursor sits on a letter.
            self.editor.col = self
                .editor
                .lines
                .get(line - 1)
                .map(|text| content_column(text))
                .unwrap_or(0);
            self.editor.clamp();
        }
        self.status = format!("edit {} of {}", index + 1, order.len()).into();
    }

    /// An agent edit that arrives while the window is open gets the same
    /// focus as one the window opened on, and says so; the person's cursor
    /// stays where they were typing.
    fn focus_new_edits(&mut self) {
        let id = self.draft_id.to_string();
        let Some(snap) = self.hub.with_session(&id, |session| session.snapshot()) else {
            return;
        };
        let order = ordered_hunks(&snap);
        let new: Vec<u64> = order
            .iter()
            .map(|(hunk, _)| *hunk)
            .filter(|hunk| !self.known_hunks.contains(hunk))
            .collect();
        self.known_hunks = order.iter().map(|(hunk, _)| *hunk).collect();
        self.seen_edits = order.len();
        if order.is_empty() {
            self.focused_hunk = None;
            return;
        }
        let still_there = self
            .focused_hunk
            .is_some_and(|focused| order.iter().any(|(hunk, _)| *hunk == focused));
        if new.is_empty() && still_there {
            return;
        }
        let first_new = new.first().copied();
        let at = first_new
            .and_then(|first| order.iter().position(|(hunk, _)| *hunk == first))
            .unwrap_or(0);
        // Until the person moves between edits, focus follows the newest
        // arrival; until they press a key, the cursor goes there too.
        if !still_there || !self.focus_by_user {
            if self.touched {
                self.focused_hunk = order.get(at).map(|(hunk, _)| *hunk);
            } else {
                self.focus_hunk_at(&order, at);
            }
        }
        if first_new.is_none() {
            return;
        }
        let who = snap
            .send_requested_by
            .as_deref()
            .map(agent_label)
            .unwrap_or_else(|| "the agent".into());
        let what = match (new.len(), order_hunk(&snap, &order, at)) {
            (1, Some(hunk)) => format!("changed {}", edit_name(&snap, hunk)),
            (count, _) => format!("made {count} edits"),
        };
        self.status = format!("{who} {what} · edit {} of {}", at + 1, order.len()).into();
    }

    /// Close the window after a send that went out.
    pub(crate) fn close_after_send(&mut self) {
        self.close_on_send = true;
    }

    /// A draft with nobody to send it to yet starts on the To line, ready
    /// to type, as a new draft from `c` does.
    fn start_on_recipients_if_empty(&mut self, snap: &Snapshot) {
        if snap.to.is_empty() && snap.cc.is_empty() && snap.bcc.is_empty() && snap.hunks.is_empty()
        {
            self.field = Field::To;
            self.status = "".into();
            // Tab from Subject carries the typing on after what is written.
            self.editor.cursor_to_end_of_text();
        }
    }

    /// An agent asked for review: start on its first edit, if any.
    pub(crate) fn start_on_first_edit(&mut self) {
        let id = self.draft_id.to_string();
        let Some(snap) = self.hub.with_session(&id, |session| session.snapshot()) else {
            return;
        };
        let order = ordered_hunks(&snap);
        self.seen_edits = order.len();
        self.known_hunks = order.iter().map(|(hunk, _)| *hunk).collect();
        if !order.is_empty() {
            self.focus_hunk_at(&order, 0);
            return;
        }
        self.start_on_recipients_if_empty(&snap);
        if snap.send_requested_by.is_none()
            && self.field == Field::Body
            && snap.in_reply_to.is_some()
            && snap.body.trim().is_empty()
        {
            // A new reply: the next thing is always typing, as after `c`.
            self.editor.row = 0;
            self.editor.col = 0;
            self.editor.mode = Mode::Insert;
            self.status = "insert".into();
            return;
        }
        if snap.send_requested_by.is_none() {
            // The person's own draft: pick up where the text ends.
            if let Some(row) = self
                .editor
                .lines
                .iter()
                .rposition(|line| !line.trim().is_empty())
            {
                self.editor.row = row;
                self.editor.col = self.editor.lines[row].chars().count();
                self.editor.clamp();
            }
            return;
        }
        // No edits yet: start on the first line of text, so the first view
        // shows headings as headings, not as the raw line under the cursor.
        if let Some(row) = self
            .editor
            .lines
            .iter()
            .position(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        {
            self.editor.row = row;
            self.editor.col = content_column(&self.editor.lines[row]);
            self.editor.clamp();
        }
    }

    fn colors(&self) -> Palette {
        match &self.look {
            Some(theme) => palette_from_theme(theme),
            None => palette(self.theme),
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn sync_from_hub(&mut self) {
        if self.closing {
            return;
        }
        self.follow_focus();
        if !self.pull_unpainted() {
            self.mark_painted();
        }
        self.focus_new_edits();
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
        // The draft this window showed is no longer in front of anyone: it
        // closes for editing, and the inbox says so, as when the window
        // closes.
        self.hub.note_hidden(self.draft_id.as_ref());
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
        self.subject_line = snap.subject.clone();
        self.sent = false;
        self.send_tried = false;
        self.header_open = false;
        self.quote_open = false;
        self.status = "".into();
        self.start_on_recipients_if_empty(&snap);
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
                self.status = "".into();
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
    /// answers. The draft is claimed for the send before it starts, so a
    /// second press, here or in a reopened window, never sends it again.
    fn send(&mut self, cx: &mut Context<Self>) {
        let id = self.draft_id.to_string();
        self.send_tried = true;
        if self.sending.as_deref() == Some(id.as_str()) {
            self.status = "Sending…".into();
            return;
        }
        if let Some(Some(lock)) = self.hub.with_session(&id, |session| session.send_lock()) {
            self.status = lock.reason().into();
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
        let prepared = self.hub.with_session(&id, prepare_from_editor);
        let prepared = match prepared {
            Some(Ok(prepared)) => prepared,
            Some(Err(err)) => {
                self.status = err.to_string().into();
                return;
            }
            None => {
                self.status = "draft is not open".into();
                return;
            }
        };
        self.sending = Some(id.clone());
        self.status = "Sending…".into();
        let hub = self.hub.clone();
        cx.spawn(async move |this, cx| {
            let sent_id = id.clone();
            let outcome = cx
                .background_executor()
                .spawn({
                    let hub = hub.clone();
                    async move { transmit_from_hub(&hub, &id, prepared, account_backend) }
                })
                .await;
            let shown = this.update(cx, |this, cx| {
                if this.sending.as_deref() == Some(sent_id.as_str()) {
                    this.sending = None;
                }
                let end = finish_send(
                    &this.hub,
                    &sent_id,
                    this.draft_id.as_ref(),
                    this.close_on_send,
                    &outcome,
                );
                if let Some(status) = end.status {
                    this.sent = end.sent;
                    this.status = status.into();
                }
                if end.close {
                    this.closing = true;
                }
                cx.notify();
            });
            if shown.is_err() {
                // The window closed while the send ran.
                hub.report_send(
                    &sent_id,
                    send_failure_line(&hub, &sent_id, &outcome).as_deref(),
                );
            }
        })
        .detach();
    }

    fn undo(&mut self) {
        let id = self.draft_id.to_string();
        let before: Vec<u64> = self
            .hub
            .with_session(&id, |session| {
                session
                    .snapshot()
                    .hunks
                    .iter()
                    .map(|hunk| hunk.id)
                    .collect()
            })
            .unwrap_or_default();
        if self.hub.with_session(&id, |session| session.undo()) == Some(true) {
            self.applied_revision = 0;
            self.sync_from_hub();
            // An edit that came back (a keep or reject undone) takes focus.
            let snap = self.hub.with_session(&id, |session| session.snapshot());
            let returned = snap.as_ref().and_then(|snap| {
                let order = ordered_hunks(snap);
                let at = order.iter().position(|(hunk, _)| !before.contains(hunk))?;
                let name = edit_name(snap, order_hunk(snap, &order, at)?);
                Some((order, at, name))
            });
            match returned {
                Some((order, at, name)) => {
                    self.focus_hunk_at(&order, at);
                    let position = format!("edit {} of {}", at + 1, order.len());
                    self.status = format!("{name} back for review · {position}").into();
                }
                None => self.status = "undone".into(),
            }
        }
    }

    fn keep_or_reject(&mut self, keep: bool) {
        self.focus_by_user = true;
        let id = self.draft_id.to_string();
        let Some(snap) = self.hub.with_session(&id, |session| session.snapshot()) else {
            return;
        };
        let order = ordered_hunks(&snap);
        let at = self
            .focused_hunk
            .and_then(|focused| order.iter().position(|(hunk, _)| *hunk == focused))
            .unwrap_or(0);
        let Some((hunk_id, _)) = order.get(at).copied() else {
            return;
        };
        let name = order_hunk(&snap, &order, at)
            .map(|hunk| edit_name(&snap, hunk))
            .unwrap_or_else(|| "the edit".into());
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
        // The next edit slides into this one's place.
        let after = self
            .hub
            .with_session(&id, |session| ordered_hunks(&session.snapshot()))
            .unwrap_or_default();
        let done = if keep { "kept" } else { "rejected" };
        if after.is_empty() {
            self.focused_hunk = None;
            self.status = format!("{done} {name} · u undo · all reviewed").into();
        } else {
            self.focus_hunk_at(&after, at.min(after.len() - 1));
            self.status = format!("{done} {name} · u undo").into();
        }
    }

    /// An agent edit can land between frames. Rebase the editor on it before
    /// a key changes the buffer, or the stale text would overwrite it.
    /// Returns false when the cursor's line is gone from the new text, so
    /// the key must not be applied anywhere.
    fn pull_if_stale(&mut self) -> bool {
        let id = self.draft_id.to_string();
        let revision = self.hub.with_session(&id, |session| session.revision());
        if !revision.is_some_and(|revision| revision != self.applied_revision) {
            return true;
        }
        let before = self.editor.lines.get(self.editor.row).cloned();
        self.pull_unpainted();
        match before {
            Some(line) => self.editor.follow_line(&line),
            None => true,
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.touched = true;
        if self.field == Field::Body && !self.pull_if_stale() {
            self.status = "the draft changed under the cursor; the key was not applied".into();
            return;
        }
        let key = compose_key(&event.keystroke);
        let key = key.as_str();
        // A draft being sent, or already sent, is read-only: what is on
        // screen must stay what went out.
        let id = self.draft_id.to_string();
        let typed =
            event.keystroke.key_char.as_deref().filter(|_| {
                !event.keystroke.modifiers.control && !event.keystroke.modifiers.platform
            });
        if let Some(Some(_)) = self.hub.with_session(&id, |session| session.send_lock())
            && key != "escape"
        {
            // Still a window: `:` and its commands work, so `:q` closes
            // it; `run_action` refuses the ones that would change the draft.
            if self.palette.is_some() {
                self.palette_key(key, typed, cx);
            } else if key == ":" {
                self.palette = Some(String::new());
            } else {
                // Shown as the idle hint for this lock; the bar's right side
                // already gives the reason.
                self.status = "normal".into();
            }
            return;
        }
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
        let locked = self
            .hub
            .with_session(self.draft_id.as_ref(), |session| session.send_lock())
            .flatten();
        if let Some(lock) = locked
            && matches!(action, "quote" | "keep" | "reject" | "undo")
        {
            self.status = lock.reason().into();
            return;
        }
        match action {
            "unlock" => {
                let released = self
                    .hub
                    .with_session(self.draft_id.as_ref(), |session| {
                        session.release_maybe_sent()
                    })
                    .unwrap_or_else(|| Err("this draft is not open".into()));
                self.status = match released {
                    // The bar's right side already offers send.
                    Ok(()) => "unlocked".into(),
                    Err(err) => err.into(),
                };
                cx.notify();
            }
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
                self.look = None;
                self.theme = match self.theme {
                    ThemeKind::Dark => ThemeKind::Light,
                    ThemeKind::Light => ThemeKind::Dark,
                };
            }
            "keep" => self.keep_or_reject(true),
            "reject" => self.keep_or_reject(false),
            "undo" => self.undo(),
            "close" => {
                // The draft is already saved; closing only hides it.
                self.flush_now();
                self.closing = true;
                // From now on a request_send must wait for a new window.
                self.hub.note_hidden(self.draft_id.as_ref());
                cx.notify();
            }
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
                    self.field = leave_header(field, &mut self.editor);
                    if self.field == Field::Body {
                        self.status = "insert".into();
                    }
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
        if let Some(bracket) = self.bracket.take() {
            if typed == Some("h") || key == "h" {
                self.goto_hunk(bracket == ']');
            }
            return;
        }
        if matches!(typed, Some("]") | Some("[")) || matches!(key, "]" | "[") {
            self.bracket = typed.or(Some(key)).and_then(|text| text.chars().next());
            return;
        }
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
                self.status = "".into();
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
        self.editor.caret_hidden = self.field != Field::Body;
        self.header_open |= self.field != Field::Body;
        if self.closing {
            window.remove_window();
        }
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
        let colors = self.colors();
        let ui_family = self
            .look
            .as_ref()
            .and_then(|theme| theme.ui_family.clone())
            .unwrap_or_else(|| "Liberation Sans".into());
        let wide = window.viewport_size().width > px(640.);
        let lock = self
            .hub
            .with_session(self.draft_id.as_ref(), |session| session.send_lock())
            .flatten();
        let mode = match self.editor.mode {
            // Nothing to type into: a narrow bar gives the mode's room to
            // the way forward and the warning.
            _ if lock.is_some() && !wide => "",
            // A header field takes typing as insert mode does.
            _ if self.field != Field::Body => "insert",
            Mode::Normal => "normal",
            Mode::Insert => "insert",
            Mode::Visual => "visual",
        };
        // The mode word says it all; how to change mode is not news.
        let hint: SharedString = match (idle_status(&self.status), lock) {
            // Nothing to type into: the way out is the hint.
            (true, Some(lock)) => locked_hint(lock, wide).into(),
            (true, None) => "".into(),
            (false, _) => self.status.clone(),
        };
        let mono = self
            .look
            .as_ref()
            .and_then(|theme| theme.mono_family.clone());
        div()
            .id("compose")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.on_key(event, cx);
                // Every key can change what is on screen (cursor, focused
                // edit, status), not only the ones that touch the draft.
                cx.notify();
            }))
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(rgb(colors.bg))
            .text_color(rgb(colors.fg))
            .text_size(px(16.))
            .font_weight(FontWeight(400.))
            .font_family(ui_family)
            .child(header(&snap, colors).flex_shrink_0())
            .child(
                self.fields_view(&snap, colors, wide, window)
                    .flex_shrink_0(),
            )
            .child(self.body_view(&snap, colors))
            .children(
                self.quote_view(&snap, colors)
                    .map(|quote| quote.flex_shrink_0()),
            )
            .child(attachment_row(&snap, colors, wide).flex_shrink_0())
            .child(
                status_bar(mode, &hint, &snap, self.send_tried, colors, wide, mono).flex_shrink_0(),
            )
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
    fn fields_view(
        &self,
        snap: &Snapshot,
        colors: Palette,
        wide: bool,
        window: &Window,
    ) -> gpui::Div {
        let mut column = div().flex().flex_col().bg(rgb(colors.panel)).px_4().py_1();
        let redirects = redirects(snap);
        // A context row's subject gives way, with an ellipsis, so who and
        // when stay on one line at any width.
        // The padding, border, label and gap around the value, and a
        // margin, since the shaped width runs a little under the painted.
        let row_px = f32::from(window.viewport_size().width) - 32.0 - 10.0 - LABEL_PX - 12.0 - 24.0;
        let fit = |who: &str, what: &str, when: &str| {
            let room = row_px - text_px(window, who, 16.0) - text_px(window, when, 16.0);
            fit_px(what, room, &|text| text_px(window, text, 16.0))
        };
        let none = HashSet::new();
        // An empty To says what it needs, in the error colour while it
        // blocks send.
        let to_hint =
            if shown_send_block(snap, self.send_tried).map(String::as_str) == Some(NO_RECIPIENT) {
                ("add a recipient", colors.error)
            } else {
                ("add a recipient", colors.dim)
            };
        column = field_row(
            column,
            "To",
            &self.to_line,
            self.field == Field::To,
            &snap.to,
            colors,
            wide,
            &redirects,
            Some(to_hint),
        );
        let copies_empty = snap.cc.is_empty() && snap.bcc.is_empty();
        // Once the header has the keyboard, Cc and Bcc keep a row each, so
        // tabbing through them does not move the body.
        let copies_open = self.header_open;
        if copies_empty && !copies_open {
            // Two empty rows would push the message down; tab still reaches each.
            column = column.child(
                div()
                    .flex()
                    .gap_3()
                    .px_1()
                    .py_1()
                    .border_l(px(2.))
                    .border_color(rgb(colors.panel))
                    .text_size(px(16.))
                    .child(div().w(px(LABEL_PX)).flex_shrink_0().child("Cc, Bcc"))
                    .child(div().text_color(rgb(colors.dim)).child("none")),
            );
        } else {
            column = field_row(
                column,
                "Cc",
                &self.cc_line,
                self.field == Field::Cc,
                &snap.cc,
                colors,
                wide,
                &none,
                Some(("none", colors.dim)),
            );
            column = field_row(
                column,
                "Bcc",
                &self.bcc_line,
                self.field == Field::Bcc,
                &snap.bcc,
                colors,
                wide,
                &none,
                Some(("none", colors.dim)),
            );
        }
        column
            .child(subject_row(
                &self.subject_line,
                self.field == Field::Subject,
                colors,
            ))
            // A new message of the person's own has nothing to reply to;
            // an agent's draft always says what it answers, even nothing.
            .when(
                snap.send_requested_by.is_some()
                    || snap.reply_context.is_some()
                    || snap
                        .in_reply_to
                        .as_deref()
                        .is_some_and(|id| !id.trim().is_empty()),
                |column| column.child(reply_row(snap, &redirects, colors, &fit)),
            )
            .children(snap.forwarded.as_ref().map(|forward| {
                let who = display_safe(&forward.sender);
                let when = forward.date_wall.map(|wall| {
                    let date = wall - crate::present::local_offset(wall);
                    format!(
                        "\u{a0}({})",
                        crate::present::relative_date(now_unix_secs(), date)
                    )
                });
                let what = fit(
                    &who,
                    &dot_before(&display_safe(&forward.subject)),
                    when.as_deref().unwrap_or(""),
                );
                context_row("Forwarding", &who, &what, when, colors)
            }))
            .children(if !snap.hunks.is_empty() {
                // Room for the edit's words: the row, less the keys beside
                // them on a wide window.
                let width = f32::from(window.viewport_size().width) - 64.0;
                let keys = if wide {
                    text_px(
                        window,
                        "  ·  ]h [h move  ·  u undo  ·  space a k keep  ·  space a x reject",
                        16.0,
                    )
                } else {
                    0.0
                };
                let room = width - keys;
                let fits = |label: &str| text_px(window, label, 16.0) <= room;
                Some(hunk_banner(snap, self.focused_hunk, colors, wide, &fits))
            } else if snap.send_requested_by.is_some() {
                // The row stays, so an edit arriving later does not push the
                // message down under the reader.
                Some(
                    div()
                        .px_1()
                        .py_1()
                        .border_l(px(2.))
                        .border_color(rgb(colors.panel))
                        .text_size(px(16.))
                        .text_color(rgb(colors.dim))
                        .child("no agent edits to review"),
                )
            } else {
                None
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
                styled_body(
                    &self.editor,
                    &self.folder,
                    &body_edit_ranges(snap),
                    self.focused_hunk,
                    draft_links(snap),
                    colors,
                )
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
        let what = if snap.forwarded.is_some() {
            "Forwarded message"
        } else {
            "Quoted original"
        };
        let label = if self.quote_open {
            format!("{what}  ·  space m q hide")
        } else {
            format!("{what}  ·  space m q show")
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
            block = block.child(render_doc(
                &html_to_doc(html),
                &self.folder,
                Links::Target,
                colors,
            ));
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
            self.status = if self.quote_open {
                "quote shown"
            } else {
                "quote hidden"
            }
            .into();
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

/// Why a send from the editor did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendFailure {
    /// Another send of this draft is running, or one went or may have gone out.
    Locked(SendLock),
    /// Nothing was transmitted. The draft can be sent again.
    NotSent(String),
    /// The connection failed after the server had the whole message. The
    /// draft stays locked so it is not sent twice.
    MaybeSent(String),
}

impl SendFailure {
    fn from_core(err: fell_core::Error) -> Self {
        match err {
            fell_core::Error::SendUncertain(detail) => Self::MaybeSent(detail),
            other => Self::NotSent(other.to_string()),
        }
    }
}

impl std::fmt::Display for SendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Locked(lock) => f.write_str(lock.reason()),
            Self::NotSent(detail) => write!(f, "Not sent: {detail}"),
            Self::MaybeSent(detail) => {
                write!(
                    f,
                    "May have been sent: {detail}. Check Sent before sending again"
                )
            }
        }
    }
}

/// What the compose window does once a send of `sent` finished, while it
/// shows `on_screen`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SendEnd {
    /// The window's new status line. `None` leaves the draft on screen alone.
    pub status: Option<String>,
    /// The window shows sent mail.
    pub sent: bool,
    /// Close the window: it shows the draft that went out.
    pub close: bool,
}

/// Said when a send went out but the mailbox could not record it.
const SENT_UNRECORDED: &str =
    "Sent; the mailbox did not record it, so this draft will read as possibly sent";

/// The window is one, and `c`, Enter or an agent's `request_send` can move
/// it to another draft while a send is still in flight. Only the draft that
/// was sent is closed, and its outcome reaches the inbox even when the
/// window no longer shows it.
pub(crate) fn finish_send(
    hub: &DraftHub,
    sent: &str,
    on_screen: &str,
    close_on_send: bool,
    outcome: &Result<fell_core::SentMessage, SendFailure>,
) -> SendEnd {
    if sent != on_screen {
        hub.report_send(sent, send_failure_line(hub, sent, outcome).as_deref());
        return SendEnd {
            status: None,
            sent: false,
            close: false,
        };
    }
    match outcome {
        Ok(_) => {
            let unrecorded = send_unrecorded(hub, sent);
            // Back to the inbox, which says it was sent. A send the mailbox
            // did not record keeps the window, so the person reads why.
            let close = close_on_send && !unrecorded;
            if close {
                hub.note_hidden(sent);
            }
            SendEnd {
                status: Some(
                    if unrecorded {
                        SENT_UNRECORDED
                    } else {
                        "Sent from the editor"
                    }
                    .into(),
                ),
                sent: true,
                close,
            }
        }
        Err(err) => SendEnd {
            status: Some(err.to_string()),
            sent: matches!(err, SendFailure::MaybeSent(_)),
            close: false,
        },
    }
}

/// The mailbox could not record how the send of `id` ended.
fn send_unrecorded(hub: &DraftHub, id: &str) -> bool {
    hub.with_session(id, |session| {
        session
            .snapshot()
            .notices
            .iter()
            .any(|notice| notice.starts_with("could not record how this send ended"))
    })
    .unwrap_or(false)
}

/// What went wrong with a send of `id`, for the inbox. `None` for a send
/// that went out and was recorded.
fn send_failure_line(
    hub: &DraftHub,
    id: &str,
    outcome: &Result<fell_core::SentMessage, SendFailure>,
) -> Option<String> {
    match outcome {
        // Worded as the inbox's other notices are: lowercase, outcome first.
        Ok(_) if send_unrecorded(hub, id) => Some(
            "sent  ·  the mailbox did not record it, so this draft reads as possibly sent".into(),
        ),
        Ok(_) => None,
        Err(SendFailure::MaybeSent(_)) => Some(fell_core::compose::MAYBE_SENT_NOTICE.into()),
        Err(SendFailure::NotSent(detail)) => Some(format!("not sent  ·  {detail}")),
        Err(SendFailure::Locked(lock)) => Some(lock.reason().into()),
    }
}

/// The lock a finished send leaves on its draft.
fn lock_after(result: &Result<fell_core::SentMessage, SendFailure>) -> Option<SendLock> {
    match result {
        Ok(_) => Some(SendLock::Sent),
        Err(SendFailure::MaybeSent(_)) => Some(SendLock::MaybeSent),
        // A refused claim leaves the other send's lock in place.
        Err(SendFailure::Locked(lock)) => Some(*lock),
        Err(SendFailure::NotSent(_)) => None,
    }
}

/// Check and send the draft on screen in one call. Blocks on SMTP, so the
/// window uses [`prepare_from_editor`] and transmits off its thread.
pub fn send_from_editor(
    session: &mut fell_core::compose::DraftSession,
    backend: &dyn MailBackend,
) -> Result<fell_core::SentMessage, SendFailure> {
    let mut prepared = prepare_from_editor(session)?;
    let sent_bcc = prepared.sent_bcc.take();
    // This call holds the session throughout, so the claim cannot be lost.
    let result = prepared.transmit_checked(backend, || true);
    session.end_send(lock_after(&result));
    remember_bcc(sent_bcc, &result);
    result
}

/// The message the editor is about to send, checked and built while the
/// draft lock is held, so what was on screen is what goes out.
pub struct PreparedSend {
    account: fell_core::AccountId,
    outgoing: fell_core::compose::Outgoing,
    files: Vec<fell_core::SubmittedFile>,
    /// Recorded after the server has, or may have, the message.
    sent_bcc: Option<fell_core::compose::SentBcc>,
}

/// Run the editor's send checks, read the attachments, and claim the draft
/// for this send. No network. Every success must be followed by
/// [`fell_core::compose::DraftSession::end_send`].
pub fn prepare_from_editor(
    session: &mut fell_core::compose::DraftSession,
) -> Result<PreparedSend, SendFailure> {
    if let Some(lock) = session.send_lock() {
        return Err(SendFailure::Locked(lock));
    }
    let outgoing = session.prepare_send().map_err(SendFailure::NotSent)?;
    let files = session
        .resolved_files()
        .into_iter()
        .map(|file| fell_core::SubmittedFile {
            name: file.display_name,
            bytes: file.bytes,
            inline: file.inline,
        })
        .collect();
    let sent_bcc = session.sent_bcc(&outgoing);
    if let Err(err) = session.begin_send() {
        // A lock only the mailbox knew of stays a lock; anything else (the
        // claim could not be recorded) sent nothing.
        return Err(match session.send_lock() {
            Some(lock) => SendFailure::Locked(lock),
            None => SendFailure::NotSent(err),
        });
    }
    Ok(PreparedSend {
        account: session.account_id().clone(),
        outgoing,
        files,
        sent_bcc,
    })
}

/// The backend for a send from the window: the demo mailbox, or the account
/// file's login and SMTP settings for this account id. Runs
/// `password_command`, so call it off the UI thread.
fn account_backend(account: &AccountId) -> Result<Box<dyn MailBackend + Send>, SendFailure> {
    let config = crate::mailbox::send_account_config(account).map_err(SendFailure::NotSent)?;
    fell_core::backend_for(account, config.as_ref()).map_err(SendFailure::from_core)
}

/// Transmit a send [`prepare_from_editor`] claimed and record how it ended on
/// the draft, even if the window closed meanwhile. Blocks on SMTP.
pub(crate) fn transmit_from_hub(
    hub: &DraftHub,
    id: &str,
    mut prepared: PreparedSend,
    backend: impl FnOnce(&AccountId) -> Result<Box<dyn MailBackend + Send>, SendFailure>,
) -> Result<fell_core::SentMessage, SendFailure> {
    let sent_bcc = prepared.sent_bcc.take();
    // While the lock is held every edit path is refused, so the prepared
    // message is still the draft. Only a send that lost its lock stops.
    let result = backend(&prepared.account).and_then(|backend| {
        prepared.transmit_checked(backend.as_ref(), || {
            hub.with_session(id, |session| session.send_lock())
                .flatten()
                == Some(SendLock::InFlight)
        })
    });
    hub.with_session(id, |session| session.end_send(lock_after(&result)));
    remember_bcc(sent_bcc, &result);
    result
}

/// Keep the Bcc of a message the server has, or may have, so the Sent copy
/// shows it once it syncs. A message that was not sent leaves no record.
/// The write runs on its own thread with a few retries: the mail already
/// went out, so a busy mailbox must neither hold up the send nor lose this.
fn remember_bcc(
    sent_bcc: Option<fell_core::compose::SentBcc>,
    result: &Result<fell_core::SentMessage, SendFailure>,
) {
    if !matches!(result, Ok(_) | Err(SendFailure::MaybeSent(_))) {
        return;
    }
    let Some(record) = sent_bcc else {
        return;
    };
    std::thread::spawn(move || {
        let mut last = None;
        for attempt in 0..4 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_secs(2 * attempt));
            }
            match record.save() {
                Ok(()) => return,
                Err(err) => last = Some(err),
            }
        }
        if let Some(err) = last {
            eprintln!("fell: could not record the Bcc of a sent message: {err}");
        }
    });
}

impl PreparedSend {
    /// Submit and confirm through `backend`. Blocks on SMTP. `still_claimed`
    /// is asked right before the message is handed to SMTP; if this send
    /// no longer holds the draft's lock, nothing is sent.
    fn transmit_checked(
        self,
        backend: &dyn MailBackend,
        still_claimed: impl FnOnce() -> bool,
    ) -> Result<fell_core::SentMessage, SendFailure> {
        let pending = backend
            .submit_composed(self.outgoing.draft, self.outgoing.mime, self.files)
            .map_err(SendFailure::from_core)?;
        if !still_claimed() {
            return Err(SendFailure::NotSent(
                "the draft is no longer being sent; nothing was sent".into(),
            ));
        }
        let confirm = confirmation_from_ui(&pending);
        backend
            .confirm_send(pending, confirm)
            .map_err(SendFailure::from_core)
    }
}

fn header(snap: &Snapshot, colors: Palette) -> gpui::Div {
    let title = div()
        .flex()
        .flex_wrap()
        .gap_x_2()
        .min_w(px(0.))
        .child(div().flex_shrink_0().font_weight(FontWeight(620.)).child(
            if snap.send_requested_by.is_some() {
                "Review and send"
            } else {
                "Compose"
            },
        ))
        .children(snap.send_requested_by.as_ref().map(|agent| {
            div()
                .text_color(rgb(colors.accent))
                .child(format!("asked by {}", agent_label(agent)))
        }));
    div()
        .flex()
        .justify_between()
        .gap_3()
        .px_4()
        .py_2()
        .bg(rgb(colors.panel))
        .border_b_1()
        .border_color(rgb(colors.selection))
        .child(title)
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(16.))
                .text_color(rgb(colors.dim))
                .child(format!("from {}", snap.from)),
        )
}

/// An agent id as the header shows it: without the `agent:` prefix, and
/// without format characters (bidi overrides, zero-width marks) that could
/// make the sentence around it read differently.
fn agent_label(agent: &str) -> String {
    display_safe(agent.strip_prefix("agent:").unwrap_or(agent))
}

/// Text from outside (an agent name, a sender, a subject) without line
/// breaks or the invisible marks that reorder or hide what is drawn.
fn display_safe(text: &str) -> String {
    text.chars()
        .filter(|ch| {
            !ch.is_control()
                && !matches!(
                    *ch,
                    '\u{200B}'..='\u{200F}'
                        | '\u{202A}'..='\u{202E}'
                        | '\u{2060}'..='\u{2069}'
                        | '\u{FEFF}'
                )
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn field_row(
    parent: gpui::Div,
    label: &str,
    value: &str,
    active: bool,
    addresses: &[fell_core::compose::AddressView],
    colors: Palette,
    wide: bool,
    redirects: &HashSet<String>,
    placeholder: Option<(&str, u32)>,
) -> gpui::Div {
    // While a field is being edited it shows the raw line. Otherwise each
    // address stands alone, with its note beside it: "(first time)" for one
    // the account never mailed, "(Reply-To)" for one the original asked
    // replies to go to instead of its sender.
    let note = |address: &fell_core::compose::AddressView| {
        let redirect = redirects.contains(&address.email.trim().to_lowercase());
        match (redirect, address.fresh) {
            (true, true) => Some(("(Reply-To · first time)", colors.error)),
            (true, false) => Some(("(Reply-To)", colors.dim)),
            (false, true) => Some(("(first time)", colors.dim)),
            (false, false) => None,
        }
    };
    // The address, then its note as one unbreakable piece: a narrow window
    // wraps the note whole onto the next line, never inside it.
    let marked = |address: &fell_core::compose::AddressView, comma: bool| {
        let comma = if comma { "," } else { "" };
        let mut piece = div().flex().flex_wrap().gap_x(px(5.));
        match note(address) {
            Some((words, color)) => {
                // The address may wrap inside itself when it alone is wider
                // than the row; the note then takes the next line.
                piece = piece
                    .child(div().min_w(px(0.)).child(address.raw.clone()))
                    .child(
                        div()
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .italic()
                            .text_color(rgb(color))
                            .child(format!("{words}{comma}")),
                    );
            }
            None => piece = piece.child(format!("{}{comma}", address.raw)),
        }
        piece
    };
    let shown = if active {
        // The field being typed in shows the raw line and a caret at its end;
        // empty, it says what it wants, dimmed.
        div()
            .flex()
            .items_center()
            .child(SharedString::from(value.to_string()))
            .child(caret(colors, Mode::Insert))
            .when(value.is_empty() && label == "To", |line| {
                line.children(placeholder.map(|(words, _)| {
                    div()
                        .pl(px(6.))
                        .text_color(rgb(colors.dim))
                        .child(words.to_string())
                }))
            })
    } else if addresses.is_empty() {
        match (value.is_empty(), placeholder) {
            (true, Some((words, color))) => div().text_color(rgb(color)).child(words.to_string()),
            (true, None) => div().child(SharedString::from(" ")),
            (false, _) => div().child(SharedString::from(value.to_string())),
        }
    } else if !wide {
        // Narrow: one address per line, its note always beside it.
        div().flex().flex_col().children(
            addresses
                .iter()
                .map(|address| div().child(marked(address, false))),
        )
    } else {
        // A wrapping row with each address whole, so a long list breaks
        // between addresses, never inside one.
        let last = addresses.len().saturating_sub(1);
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_x_2()
            .children(addresses.iter().enumerate().map(|(index, address)| {
                div()
                    .whitespace_nowrap()
                    .child(marked(address, index < last))
            }))
    };
    parent.child(
        div()
            .flex()
            .gap_3()
            .px_1()
            .py_1()
            .border_l(px(2.))
            .border_color(rgb(if active { colors.accent } else { colors.panel }))
            .child(
                div()
                    .w(px(LABEL_PX))
                    .flex_shrink_0()
                    .text_size(px(16.))
                    .text_color(rgb(colors.fg))
                    .child(label.to_string()),
            )
            .child(shown.flex_1().min_w(px(0.))),
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
                .w(px(LABEL_PX))
                .flex_shrink_0()
                .text_size(px(16.))
                .text_color(rgb(colors.fg))
                .child("Subject"),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .font_weight(FontWeight(560.))
                .flex()
                .items_center()
                .child(subject.to_string())
                .when(active, |row| row.child(caret(colors, Mode::Insert))),
        )
}

/// The header's label column: wide enough for "Forwarding" on one line.
const LABEL_PX: f32 = 100.0;

/// Why send is blocked when To is empty.
const NO_RECIPIENT: &str = "add at least one recipient";

/// To addresses the original's Reply-To sent the reply to instead of its
/// sender. Empty when the reply goes to the sender.
fn redirects(snap: &Snapshot) -> HashSet<String> {
    let Some(parent) = &snap.reply_context else {
        return HashSet::new();
    };
    let sender = parent.sender_email.trim().to_lowercase();
    parent
        .reply_to
        .iter()
        .filter(|email| **email != sender)
        .cloned()
        .collect()
}

fn reply_row(
    snap: &Snapshot,
    redirects: &HashSet<String>,
    colors: Palette,
    fit: &dyn Fn(&str, &str, &str) -> String,
) -> gpui::Div {
    match (&snap.reply_context, snap.in_reply_to.as_deref()) {
        (Some(parent), _) => {
            // When Reply-To sends the answer elsewhere, the sender's own
            // address shows, so the difference from To is plain.
            let redirected = snap
                .to
                .iter()
                .any(|address| redirects.contains(&address.email.trim().to_lowercase()));
            let sender = if redirected && parent.sender_email != parent.sender {
                format!("{} <{}>", parent.sender, parent.sender_email)
            } else {
                parent.sender.clone()
            };
            let who = display_safe(&sender);
            let when = format!(
                "\u{a0}({})",
                crate::present::relative_date(now_unix_secs(), parent.date_unix)
            );
            let subject = display_safe(&parent.subject);
            let what = fit(&who, &dot_before(&subject), &when);
            if what.chars().count() < 12 && who != parent.sender {
                // Too little room beside the sender's address: the subject
                // and date take the next line.
                let what = fit("", &subject, &when);
                return context_lines("In reply to", &who, "", None, Some((what, when)), colors);
            }
            context_row("In reply to", &who, &what, Some(when), colors)
        }
        (None, Some(id)) if !id.trim().is_empty() => {
            context_row("In reply to", &display_safe(id), "", None, colors)
        }
        _ => context_row("In reply to", "none (new message)", "", None, colors),
    }
}

/// " · subject", joined so it never breaks from what comes before.
fn dot_before(text: &str) -> String {
    format!("\u{a0}·\u{a0}{text}")
}

/// `text`, cut with an ellipsis so it measures at most `max_px`. Empty when
/// not even the ellipsis fits.
fn fit_px(text: &str, max_px: f32, measure: &dyn Fn(&str) -> f32) -> String {
    if measure(text) <= max_px {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    for keep in (0..chars.len()).rev() {
        let cut: String = chars[..keep].iter().collect();
        let candidate = format!("{}…", cut.trim_end());
        if measure(&candidate) <= max_px {
            return candidate;
        }
    }
    String::new()
}

/// "In reply to" or "Forwarding": who, then what, then when. The subject
/// comes fitted, so the date stays on the line in a narrow window.
fn context_row(
    label: &str,
    who: &str,
    what: &str,
    when: Option<String>,
    colors: Palette,
) -> gpui::Div {
    context_lines(label, who, what, when, None, colors)
}

/// One line of who, what and when; or, with `below`, who alone and then
/// what and when on the next line.
fn context_lines(
    label: &str,
    who: &str,
    what: &str,
    when: Option<String>,
    below: Option<(String, String)>,
    colors: Palette,
) -> gpui::Div {
    let words = |line: gpui::Div, what: &str, when: Option<String>| {
        let mut line = line;
        if !what.is_empty() {
            line = line.child(
                div()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(what.to_string()),
            );
        }
        if let Some(when) = when {
            line = line.child(div().flex_shrink_0().child(when));
        }
        line
    };
    let first = words(
        div()
            .flex()
            .min_w(px(0.))
            .child(div().flex_shrink_0().child(who.to_string())),
        what,
        when,
    );
    let mut value = div().flex().flex_col().flex_1().min_w(px(0.)).child(first);
    if let Some((what, when)) = below {
        value = value.child(words(div().flex().min_w(px(0.)), &what, Some(when)));
    }
    div()
        .flex()
        .gap_3()
        .px_1()
        .py_1()
        .border_l(px(2.))
        .border_color(rgb(colors.panel))
        .text_size(px(16.))
        .text_color(rgb(colors.fg))
        .child(
            div()
                .w(px(LABEL_PX))
                .flex_shrink_0()
                .child(label.to_string()),
        )
        .child(value)
}

fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

fn hunk_banner(
    snap: &Snapshot,
    focused: Option<u64>,
    colors: Palette,
    wide: bool,
    fits: &dyn Fn(&str) -> bool,
) -> gpui::Div {
    let order = ordered_hunks(snap);
    let at = focused
        .and_then(|focused| order.iter().position(|(hunk, _)| *hunk == focused))
        .unwrap_or(0);
    let hunk = order
        .get(at)
        .and_then(|(id, _)| snap.hunks.iter().find(|hunk| hunk.id == *id));
    let what = hunk
        .map(|hunk| match hunk.kind {
            HunkKind::Recipients => "agent changed recipients".to_string(),
            HunkKind::Subject => "agent changed the subject".to_string(),
            HunkKind::Attachments => "agent changed attachments".to_string(),
            HunkKind::Body => {
                // As many words as the row has room for.
                let count = format!(" · {} of {}", at + 1, order.len().max(1));
                (1..=12)
                    .rev()
                    .filter_map(|words| edit_preview_words(snap, hunk, words))
                    .map(|words| format!("agent edit \u{201c}{words}\u{201d}"))
                    .find(|label| fits(&format!("{label}{count}")))
                    .or_else(|| {
                        edit_preview_words(snap, hunk, 1)
                            .map(|words| format!("agent edit \u{201c}{words}\u{201d}"))
                    })
                    .unwrap_or_else(|| "agent edit".to_string())
            }
        })
        .unwrap_or_else(|| "agent edit".into());
    let label = format!("{what} · {} of {}", at + 1, order.len().max(1));
    // Parts wrap as units, so a narrow window never cuts a key in half.
    let parts = [
        label,
        "]h [h move".to_string(),
        "u undo".to_string(),
        "space a k keep".to_string(),
        "space a x reject".to_string(),
    ];
    let last = parts.len() - 1;
    div()
        .flex()
        .flex_wrap()
        .px_1()
        .py_1()
        .border_l(px(2.))
        .border_color(rgb(colors.rule))
        .bg(rgb(colors.hunk))
        .text_size(px(16.))
        .text_color(rgb(colors.fg))
        // Wide: one line with dots. Narrow: parts wrap as units with space
        // between them, so no line ends on a dot.
        .gap_x(px(if wide { 0. } else { 18. }))
        .children(parts.into_iter().enumerate().map(|(index, part)| {
            div().flex_shrink_0().child(if wide && index < last {
                format!("{part}  ·  ")
            } else {
                part
            })
        }))
}

/// The body line (from 1) where a body hunk starts, as the editor counts it.
fn body_edit_line(snap: &Snapshot, hunk: &fell_core::compose::HunkView) -> Option<usize> {
    let (at, _) = hunk_body_range(snap, hunk)?;
    Some(snap.body[..at].matches('\n').count() + 1)
}

/// A body hunk's own range in the body, without the blank space around
/// it: mapped from where it sits in the draft file, never searched for,
/// so the same text earlier in the body is not taken for it.
fn hunk_body_range(snap: &Snapshot, hunk: &fell_core::compose::HunkView) -> Option<(usize, usize)> {
    let (start, end) = snap.body_range(hunk.start, hunk.end)?;
    let slice = snap.body.get(start..end)?;
    let trimmed = slice.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lead = slice.len() - slice.trim_start().len();
    Some((start + lead, start + lead + trimmed.len()))
}

/// How the styled body shows where links go: an agent's draft is the
/// agent's words until sent, so each link says where it goes.
fn draft_links(snap: &Snapshot) -> Links {
    if snap.agent_authored {
        Links::Host
    } else {
        Links::Label
    }
}

/// Every agent edit in reading order: header changes first, then body
/// edits by line. The line is set for body edits.
fn ordered_hunks(snap: &Snapshot) -> Vec<(u64, Option<usize>)> {
    let mut header: Vec<(u64, Option<usize>)> = Vec::new();
    let mut body: Vec<(u64, Option<usize>)> = Vec::new();
    for hunk in &snap.hunks {
        match hunk.kind {
            HunkKind::Body => body.push((hunk.id, body_edit_line(snap, hunk))),
            _ => header.push((hunk.id, None)),
        }
    }
    body.sort_by_key(|(id, line)| (line.unwrap_or(usize::MAX), *id));
    header.extend(body);
    header
}

/// The first few words of a body edit's new text, for the edit bar.
fn edit_preview(snap: &Snapshot, hunk: &fell_core::compose::HunkView) -> Option<String> {
    edit_preview_words(snap, hunk, 4)
}

/// The width of one line of text at `size` pixels, in the window's font.
fn text_px(window: &Window, text: &str, size: f32) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let style = window.text_style();
    let run = style.to_run(text.len());
    let line = window.text_system().shape_line(
        SharedString::from(text.to_string()),
        px(size),
        &[run],
        None,
    );
    f32::from(line.width)
}

/// The edit's first words, at most `limit`, with an ellipsis when cut.
fn edit_preview_words(
    snap: &Snapshot,
    hunk: &fell_core::compose::HunkView,
    limit: usize,
) -> Option<String> {
    let slice = snap.text.get(hunk.start..hunk.end)?;
    let line = slice
        .lines()
        .map(plain_markdown)
        .find(|line| !line.is_empty())?;
    let mut words: Vec<&str> = line.split_whitespace().take(limit).collect();
    let more = line.split_whitespace().count() > words.len();
    if words.is_empty() {
        return None;
    }
    if more {
        words.push("…");
    }
    Some(words.join(" "))
}

fn order_hunk<'a>(
    snap: &'a Snapshot,
    order: &[(u64, Option<usize>)],
    at: usize,
) -> Option<&'a fell_core::compose::HunkView> {
    let (id, _) = order.get(at)?;
    snap.hunks.iter().find(|hunk| hunk.id == *id)
}

/// How the status line names an edit: its first words, or what it changed.
fn edit_name(snap: &Snapshot, hunk: &fell_core::compose::HunkView) -> String {
    match hunk.kind {
        HunkKind::Recipients => "the recipient change".into(),
        HunkKind::Subject => "the subject change".into(),
        HunkKind::Attachments => "the attachment change".into(),
        HunkKind::Body => edit_preview(snap, hunk)
            .map(|words| format!("\u{201c}{words}\u{201d}"))
            .unwrap_or_else(|| "the edit".into()),
    }
}

/// Markdown source shown as plain words: list and heading markers and
/// emphasis marks dropped, so "was *March*" reads like the new text.
fn plain_markdown(text: &str) -> String {
    let line = text.trim();
    let mut line = line[content_byte(line)..].to_string();
    for mark in ["**", "__", "*", "_", "`"] {
        line = strip_paired(&line, mark);
    }
    line.trim().to_string()
}

/// Drop `mark` where it opens and closes a run of words ("*March*"), and
/// leave it inside words ("snake_case", "a*b").
fn strip_paired(text: &str, mark: &str) -> String {
    let opens = |at: usize| {
        let before = text[..at].chars().next_back();
        let after = text[at + mark.len()..].chars().next();
        before.is_none_or(|ch| !ch.is_alphanumeric()) && after.is_some_and(|ch| !ch.is_whitespace())
    };
    let closes = |at: usize| {
        let before = text[..at].chars().next_back();
        let after = text[at + mark.len()..].chars().next();
        before.is_some_and(|ch| !ch.is_whitespace()) && after.is_none_or(|ch| !ch.is_alphanumeric())
    };
    let mut out = String::new();
    let mut rest = 0;
    let mut search = 0;
    while let Some(found) = text[search..].find(mark) {
        let open = search + found;
        if !opens(open) {
            search = open + mark.len();
            continue;
        }
        let inner = open + mark.len();
        let close = text[inner..]
            .match_indices(mark)
            .map(|(at, _)| inner + at)
            .find(|at| *at > inner && closes(*at));
        let Some(close) = close else {
            break;
        };
        out.push_str(&text[rest..open]);
        out.push_str(&text[inner..close]);
        rest = close + mark.len();
        search = rest;
    }
    out.push_str(&text[rest..]);
    out
}

fn content_byte(line: &str) -> usize {
    let column = content_column(line);
    line.char_indices()
        .nth(column)
        .map(|(at, _)| at)
        .unwrap_or(line.len())
}

/// The first column after a line's markdown marker ("- ", "> ", "1. ").
fn content_column(line: &str) -> usize {
    let trimmed = line.trim_start();
    let indent = line.chars().count() - trimmed.chars().count();
    let marker = ["- ", "* ", "+ ", "> ", "# ", "## ", "### "]
        .iter()
        .find(|marker| trimmed.starts_with(**marker))
        .map(|marker| marker.chars().count())
        .or_else(|| {
            let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
            (digits > 0 && trimmed[digits..].starts_with(". ")).then_some(digits + 2)
        })
        .unwrap_or(0);
    indent + marker
}

/// Each body edit: its byte range in the body and the text it replaced.
fn body_edit_ranges(snap: &Snapshot) -> Vec<(usize, usize, String, u64)> {
    snap.hunks
        .iter()
        .filter(|hunk| hunk.kind == HunkKind::Body)
        .filter_map(|hunk| {
            let (start, end) = hunk_body_range(snap, hunk)?;
            Some((start, end, hunk.previous.trim().to_string(), hunk.id))
        })
        .collect()
}

fn styled_body(
    editor: &Editor,
    folder: &Path,
    hunks: &[(usize, usize, String, u64)],
    focused: Option<u64>,
    links: Links,
    colors: Palette,
) -> gpui::Div {
    let text = editor.text();
    let doc = markdown_to_doc(&text);
    let mut column = div().flex().flex_col();
    let mut previous: Option<&BlockBody> = None;
    let items: Vec<BodyItem> = if editor.caret_hidden {
        // Typing goes to a header field: every block shows styled, with
        // no caret line, so there is one caret on screen.
        (0..doc.blocks.len())
            .map(|index| BodyItem::Block { index, raw: false })
            .collect()
    } else {
        body_layout(&text, editor.row, editor.col, &doc)
    };
    for (index, item) in items.into_iter().enumerate() {
        let (body, mut painted) = match item {
            BodyItem::CaretRow => (None, cursor_line_row(editor, colors)),
            BodyItem::Block { index, raw } => {
                let block = &doc.blocks[index];
                let edits: Vec<&(usize, usize, String, u64)> = block
                    .source
                    .map(|(start, end)| {
                        hunks
                            .iter()
                            .filter(|(hunk_start, hunk_end, _, _)| {
                                start < *hunk_end && end > *hunk_start
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let painted = if raw || !edits.is_empty() {
                    raw_source_block(block, &text, editor, &edits, focused, links, colors)
                } else {
                    render_block(block, folder, links, colors)
                };
                // A block in source form spaces like text, not like an image.
                ((!raw && edits.is_empty()).then_some(&block.body), painted)
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
    raw_line(
        line,
        Some(editor.col),
        true,
        Urls::Source,
        editor.mode,
        colors,
    )
}

fn raw_body(editor: &Editor, colors: Palette) -> gpui::Div {
    let mut column = div().flex().flex_col().font_family("monospace");
    for (index, line) in editor.lines.iter().enumerate() {
        let cursor = !editor.caret_hidden && index == editor.row;
        column = column.child(raw_line(
            line,
            cursor.then_some(editor.col),
            true,
            Urls::Source,
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
    edits: &[&(usize, usize, String, u64)],
    focused: Option<u64>,
    links: Links,
    colors: Palette,
) -> gpui::Div {
    let Some((start, end)) = block.source else {
        return render_block(block, Path::new(""), links, colors);
    };
    let slice = &text[start.min(text.len())..end.min(text.len())];
    let mut column = div().flex().flex_col();
    let base_line = text[..start].chars().filter(|ch| *ch == '\n').count();
    let mut line_start = start;
    let mut announced = Vec::new();
    let mut previous_tinted = false;
    for (offset, line) in slice.trim_end_matches('\n').split('\n').enumerate() {
        let row = base_line + offset;
        let line_end = line_start + line.len();
        let cursor = (!editor.caret_hidden && row == editor.row).then_some(editor.col);
        // Only the lines an agent changed carry the tint, with what they
        // replaced directly above the first of them.
        let touching: Vec<usize> = edits
            .iter()
            .enumerate()
            .filter(|(_, (from, to, _, _))| {
                *from < line_end.max(line_start + 1) && *to > line_start
            })
            .map(|(index, _)| index)
            .collect();
        // Markers fold off the cursor line. A link's URL does too, except
        // on a line an agent changed: a new destination must be seen.
        let urls = if cursor.is_some() || !touching.is_empty() {
            Urls::Source
        } else {
            Urls::Fold(links)
        };
        let mut painted = raw_line(line, cursor, cursor.is_some(), urls, editor.mode, colors);
        if !touching.is_empty() {
            // A new edit right under another one starts its own block.
            let mut gap =
                previous_tinted && touching.iter().any(|index| !announced.contains(index));
            for index in &touching {
                if announced.contains(index) {
                    continue;
                }
                announced.push(*index);
                let previous = plain_markdown(&edits[*index].2);
                let is_focused = Some(edits[*index].3) == focused;
                if !previous.is_empty() {
                    column = column.child(
                        div()
                            .when(gap, |row| row.mt(px(6.)))
                            .ml(px(-3.))
                            .flex()
                            .gap_2()
                            .bg(rgb(if is_focused {
                                colors.hunk_focus
                            } else {
                                colors.hunk
                            }))
                            .border_l(px(3.))
                            .border_color(rgb(if is_focused {
                                colors.accent
                            } else {
                                colors.hunk
                            }))
                            .text_color(rgb(colors.dim))
                            .child(div().flex_shrink_0().child("was"))
                            .child(div().line_through().child(previous)),
                    );
                    gap = false;
                }
            }
            // The focused edit carries an accent bar, so the body shows
            // which one the bar, keep and reject are about.
            let is_focused = touching
                .iter()
                .any(|index| Some(edits[*index].3) == focused);
            // The 3px bar sits in the margin, so the text lines up with the
            // lines around the block.
            painted = div()
                .when(gap, |row| row.mt(px(6.)))
                .ml(px(-3.))
                .bg(rgb(if is_focused {
                    colors.hunk_focus
                } else {
                    colors.hunk
                }))
                .border_l(px(3.))
                .border_color(rgb(if is_focused {
                    colors.accent
                } else {
                    colors.hunk
                }))
                .child(painted);
        }
        previous_tinted = !touching.is_empty();
        column = column.child(painted);
        line_start = line_end + 1;
    }
    column
}

/// How a source line shows a link's `](url)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Urls {
    /// As written: the cursor line, the raw view, a line an agent changed.
    Source,
    /// Folded away, with the destination after the label when `Links`
    /// says the reader needs it, as the styled body shows it.
    Fold(Links),
}

/// Whether a span of a source line is left out: markers off the cursor
/// line, and a link's `](url)` when URLs fold.
fn folds(kind: StyleKind, show_markers: bool, urls: Urls) -> bool {
    match kind {
        StyleKind::Marker => !show_markers,
        StyleKind::LinkUrl => matches!(urls, Urls::Fold(_)),
        _ => false,
    }
}

/// What a source line shows after a link's `](url)` span, if anything:
/// where a folded link goes when the label alone could mislead, or an
/// IDN host in `xn--` form beside a URL shown as written.
fn url_note(label: &str, url_span: &str, urls: Urls) -> Option<String> {
    let href = span_href(url_span)?;
    match urls {
        Urls::Source => idn_host(href).map(|host| format!("({host})")),
        Urls::Fold(links) => shown_target(&plain_markdown(label), href, links),
    }
}

/// The target inside a link's `](url "title")` span.
fn span_href(span: &str) -> Option<&str> {
    let inner = span.strip_prefix("](")?.strip_suffix(')')?;
    let href = inner.split_whitespace().next()?;
    let href = href
        .strip_prefix('<')
        .and_then(|href| href.strip_suffix('>'))
        .unwrap_or(href);
    (!href.is_empty()).then_some(href)
}

fn raw_line(
    line: &str,
    cursor: Option<usize>,
    show_markers: bool,
    urls: Urls,
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
    let mut label = "";
    for span in styles {
        let slice = &line[span.start.min(line.len())..span.end.min(line.len())];
        if span.kind == StyleKind::LinkText {
            label = slice;
        }
        let note = (span.kind == StyleKind::LinkUrl)
            .then(|| url_note(label, slice, urls))
            .flatten();
        if folds(span.kind, show_markers, urls) {
            if span.start == 0 && span.kind == StyleKind::Marker {
                row = row.child(folded_marker(line, colors));
            }
            if let Some(note) = note {
                row = row.child(link_note(note, colors));
            }
            continue;
        }
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
            if let Some(note) = note {
                row = row.child(link_note(note, colors));
            }
            continue;
        }
        row = row.child(styled_span(slice, color, weight, span.kind, colors.code));
        if let Some(note) = note {
            row = row.child(link_note(note, colors));
        }
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
            .child(format!("{indent}• "));
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

fn render_doc(doc: &StyledDoc, folder: &Path, links: Links, colors: Palette) -> gpui::Div {
    let mut column = div().flex().flex_col();
    let mut previous = None;
    for block in &doc.blocks {
        let mut painted = render_block(block, folder, links, colors);
        if previous.is_some() && !joins_previous(previous, Some(&block.body)) {
            painted = painted.mt_2();
        }
        previous = Some(&block.body);
        column = column.child(painted);
    }
    column
}

/// How a styled link shows where it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Links {
    /// The draft as the recipient reads it: the label alone, as on a line
    /// the cursor is not on. A label that reads as an address but goes
    /// somewhere else still shows where.
    Label,
    /// The label, then the host it goes to, dimmed: an agent's draft,
    /// whose every link the person has not yet checked.
    Host,
    /// The label, then the destination, dimmed: the HTML preview before
    /// send and the quoted original.
    Target,
}

/// Where a link goes, dimmed after its label.
fn link_note(note: String, colors: Palette) -> gpui::Div {
    div()
        .pl(px(4.))
        .text_color(rgb(colors.dim))
        .child(display_safe(&note))
}

fn render_block(
    block: &fell_core::compose::Block,
    folder: &Path,
    links: Links,
    colors: Palette,
) -> gpui::Div {
    match &block.body {
        BlockBody::Heading { level, inlines } => div()
            .font_weight(FontWeight(680.))
            .text_size(px(if *level <= 1 { 28. } else { 22. }))
            .child(render_inlines(inlines, links, colors)),
        BlockBody::Paragraph { inlines } => match plain_lines(inlines) {
            // Plain text wraps at the window's edge, one line per break,
            // so a long quoted line in a narrow window is not cut off.
            Some(lines) => lines
                .into_iter()
                .fold(div().flex().flex_col(), |column, line| {
                    column.child(div().w_full().child(line))
                }),
            None => div().child(render_inlines(inlines, links, colors)),
        },
        BlockBody::List { ordered, items } => {
            let mut list = div().flex().flex_col();
            for (index, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    format!("{}.", index + 1)
                } else {
                    "•".into()
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
                        .child(render_inlines(item, links, colors)),
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
                quote = quote.child(render_block(block, folder, links, colors));
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

/// A paragraph that is only text, split at its line breaks. `None` when it
/// has links, emphasis, code or images, which keep their inline layout.
fn plain_lines(inlines: &[fell_core::compose::Inline]) -> Option<Vec<String>> {
    let mut text = String::new();
    for inline in inlines {
        match inline {
            fell_core::compose::Inline::Text(part) => text.push_str(part),
            _ => return None,
        }
    }
    Some(text.split('\n').map(str::to_string).collect())
}

fn render_inlines(
    inlines: &[fell_core::compose::Inline],
    links: Links,
    colors: Palette,
) -> gpui::Div {
    let mut row = div().flex().flex_row().flex_wrap();
    for inline in inlines {
        row = match inline {
            fell_core::compose::Inline::Text(text) => row.child(div().child(text.clone())),
            fell_core::compose::Inline::Strong(inner) => row.child(
                div()
                    .font_weight(FontWeight(700.))
                    .font_family("Liberation Sans")
                    .child(render_inlines(inner, links, colors)),
            ),
            fell_core::compose::Inline::Em(inner) => {
                row.child(div().italic().child(render_inlines(inner, links, colors)))
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
                .children(shown_target(text, href, links).map(|target| link_note(target, colors))),
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

fn attachment_row(snap: &Snapshot, colors: Palette, wide: bool) -> gpui::Div {
    let own = snap
        .attachments
        .iter()
        .map(|file| {
            if file.inline {
                format!("{} inline", file.name)
            } else {
                file.name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("  ·  ");
    // A forward cannot carry the original's files yet: say so here, open
    // quote or not, so no one sends it thinking they went along.
    let dropped = snap
        .forwarded
        .as_ref()
        .and_then(|forward| forward.not_forwarded.as_deref())
        .map(|names| format!("{} not forwarded", display_safe(names)));
    let row = div()
        .flex()
        .when(!wide, |row| row.flex_col())
        .min_w(px(0.))
        .px_5()
        .py_2()
        .text_size(px(16.))
        .bg(rgb(colors.panel))
        .text_color(rgb(colors.fg))
        .border_t_1()
        .border_color(rgb(colors.rule))
        .child(
            div()
                .flex_shrink_0()
                .child(if own.is_empty() && dropped.is_some() {
                    "Attachments\u{a0}\u{a0}".to_string()
                } else if own.is_empty() {
                    "Attachments\u{a0}\u{a0}none".to_string()
                } else {
                    format!("Attachments\u{a0}\u{a0}{own}")
                }),
        );
    // The dropped files are the value when the draft has none of its own;
    // beside files it has, they follow a dot. A narrow window gives them a
    // line of their own rather than cut them.
    row.children(dropped.map(|dropped| {
        let lead = if own.is_empty() || !wide {
            ""
        } else {
            "\u{a0}\u{a0}·\u{a0}\u{a0}"
        };
        div()
            .min_w(px(0.))
            .text_color(rgb(colors.error))
            .child(format!("{lead}{dropped}"))
    }))
}

/// Why send is blocked, as the status bar says it. A new draft has nobody
/// to send to yet: that is a block only once the person tries to send it.
fn shown_send_block(snap: &Snapshot, send_tried: bool) -> Option<&String> {
    snap.send_block
        .as_ref()
        .filter(|reason| send_tried || reason.as_str() != NO_RECIPIENT)
}

/// A status that says nothing the mode word does not.
fn idle_status(status: &str) -> bool {
    matches!(status, "" | "normal" | "insert")
}

/// One line, in the inbox's style: mode and hints on the left, send on
/// the right, `key action` pairs separated by ` · `, in the mono face.
fn status_bar(
    mode: &str,
    status: &SharedString,
    snap: &Snapshot,
    send_tried: bool,
    colors: Palette,
    wide: bool,
    mono: Option<String>,
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
        1 => Some("1 first-time recipient".to_string()),
        count => Some(format!("{count} first-time recipients")),
    };
    let dot = || div().text_color(rgb(colors.dim)).child("·");
    // A narrow window keeps one line: the mode, its key hint, the
    // first-time count, send and the unreviewed count stay; the close hint
    // goes first.
    // Clipped, so a long message never draws under the send keys.
    let mut left = div()
        .flex()
        .flex_1()
        .gap_2()
        .min_w(px(0.))
        .overflow_hidden();
    let idle = idle_status(status);
    if wide {
        left = left
            .child(div().text_color(rgb(colors.dim)).child(mode.to_string()))
            // The message gives way first, so it never runs under send.
            .when(!idle, |left| {
                left.child(dot()).child(
                    div()
                        .min_w(px(0.))
                        .truncate()
                        .text_color(rgb(colors.fg))
                        .child(status.to_string()),
                )
            });
        // The header tags each first-time address; the count only fills
        // an idle bar, so it never crowds out a message.
        if let Some(label) = fresh_label.filter(|_| idle) {
            left = left.child(dot()).child(
                div()
                    .flex_shrink_0()
                    .text_color(rgb(colors.fg))
                    .child(label),
            );
        }
    } else {
        // Narrow: the mode always shows; a message (an edit position, an
        // error) beats the hints and gives way first.
        if !mode.is_empty() {
            left = left
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(rgb(colors.dim))
                        .child(mode.to_string()),
                )
                .when(!idle, |left| left.child(dot()));
        }
        if !idle {
            left = left.child(
                div()
                    .min_w(px(0.))
                    .truncate()
                    .text_color(rgb(colors.fg))
                    .child(status.to_string()),
            );
        }
        // The header tags each first-time address; the count fills an idle
        // bar only, so it is never read as a count of edits.
        if fresh > 0 && idle {
            left = left.child(dot()).child(
                div()
                    .flex_shrink_0()
                    .text_color(rgb(colors.fg))
                    // Short enough to sit beside the mode and send, in the
                    // words the header and a wide bar use.
                    .child(format!("first-time: {fresh}")),
            );
        }
    }
    let right = match shown_send_block(snap, send_tried) {
        // Gone out: say so plainly, not as a block.
        Some(_) if snap.send_lock_reason() == Some(SendLock::Sent.reason()) => div()
            .flex()
            .flex_shrink_0()
            .gap_2()
            .child(
                div()
                    .text_color(rgb(colors.accent))
                    .font_weight(FontWeight(600.))
                    .child("sent"),
            )
            .child(dot())
            .child(div().text_color(rgb(colors.fg)).child(":q close")),
        // Narrow: the reason alone says send is blocked, and leaves the
        // message on the left its room.
        // May have gone out: the warning the inbox gives, in its colour.
        Some(reason) if reason.as_str() == SendLock::MaybeSent.reason() => div()
            .flex_shrink_0()
            .text_color(rgb(colors.error))
            .child(MAYBE_SENT_WARNING),
        // Narrow: a missing recipient says so in two words, so "send
        // blocked" stays and reads as a block, not advice.
        Some(reason) => div()
            .flex()
            .flex_shrink_0()
            .gap_2()
            .when(wide || reason.as_str() == NO_RECIPIENT, |right| {
                right
                    .child(div().text_color(rgb(colors.dim)).child("send blocked"))
                    .child(dot())
            })
            .child(div().text_color(rgb(colors.fg)).child(
                if !wide && reason.as_str() == NO_RECIPIENT {
                    "no recipient".to_string()
                } else {
                    reason.clone()
                },
            )),
        None => {
            let mut right = div()
                .flex()
                .flex_shrink_0()
                .gap_2()
                .child(div().text_color(rgb(colors.fg)).child("space m s send"));
            if snap.unreviewed > 0 {
                // Sending accepts these, so they stand out.
                right = right.child(dot()).child(
                    div()
                        .text_color(rgb(colors.accent))
                        .font_weight(FontWeight(600.))
                        .child(format!("{} unreviewed", snap.unreviewed)),
                );
            }
            // Narrow: the close hint goes first; wide has room for it.
            right.when(wide, |right| {
                right
                    .child(dot())
                    .child(div().text_color(rgb(colors.fg)).child(":q close"))
            })
        }
    };
    let bar = div()
        .flex()
        .items_center()
        .justify_between()
        .gap_x(px(if wide { 16. } else { 10. }))
        .px(px(16.))
        .py(px(6.))
        .bg(rgb(colors.panel))
        .border_t_1()
        .border_color(rgb(colors.rule))
        .text_size(px(if wide { 14. } else { 13. }))
        .child(left)
        .child(right);
    match mono {
        Some(family) => bar.font_family(family),
        None => bar,
    }
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
        .child(render_doc(&doc, Path::new(""), Links::Target, colors))
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
        send_requested_by: None,
        reply_context: None,
        forwarded: None,
        agent_authored: false,
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
    if href.is_empty() || href == text.trim() && idn_host(href).is_none() {
        return None;
    }
    Some(format!("({})", ascii_target(href)))
}

/// The destination a styled link shows after its label, if any.
///
/// The person's own words show the label alone, unless the label reads as
/// an address that is not where the link goes. An agent's words show the
/// host too, unless the label already names it. A host in another script
/// always shows, in its `xn--` form, so a look-alike cannot pass.
fn shown_target(text: &str, href: &str, links: Links) -> Option<String> {
    let href = href.trim();
    if href.is_empty() {
        return None;
    }
    let host = || Some(format!("({})", link_host(href)));
    match links {
        Links::Target => link_target(text, href),
        _ if idn_host(href).is_some() || label_misleads(text, href) => host(),
        Links::Host if !label_names(text, href) => host(),
        Links::Host | Links::Label => None,
    }
}

/// Where a link goes, in brief: its host (`xn--` for an IDN), the address
/// of a `mailto:`, or else the whole target.
fn link_host(href: &str) -> String {
    match place_of_href(href) {
        Some(Place::Host(host)) => host,
        Some(Place::Mail { address, .. }) => address,
        None => ascii_target(href),
    }
}

/// The whole target, an IDN host in `xn--` form.
fn ascii_target(href: &str) -> String {
    let href = href.trim();
    if href.is_ascii() {
        return href.to_string();
    }
    url::Url::parse(href)
        .map(|url| url.to_string())
        .unwrap_or_else(|_| href.to_string())
}

/// The `xn--` form of a link's host, when the host is not plain ASCII.
fn idn_host(href: &str) -> Option<String> {
    let url = url::Url::parse(href.trim()).ok()?;
    let host = url.host_str()?;
    host.split('.')
        .any(|label| label.starts_with("xn--"))
        .then(|| host.to_string())
}

/// A place a label or a link names: a host, or a mail address.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Place {
    /// Lowercase, `xn--` form, without a leading `www.`.
    Host(String),
    Mail {
        address: String,
        domain: String,
    },
}

impl Place {
    fn domain(&self) -> &str {
        match self {
            Place::Host(host) => host,
            Place::Mail { domain, .. } => domain,
        }
    }
}

/// The ASCII (`xn--`) lowercase form of a host, without `www.`.
fn ascii_host(host: &str) -> Option<String> {
    let url = url::Url::parse(&format!("http://{host}/")).ok()?;
    let host = url.host_str()?.to_string();
    Some(host.strip_prefix("www.").unwrap_or(&host).to_string())
}

fn place_of_href(href: &str) -> Option<Place> {
    let url = url::Url::parse(href.trim()).ok()?;
    if url.scheme().eq_ignore_ascii_case("mailto") {
        let (local, domain) = url.path().rsplit_once('@')?;
        let domain = ascii_host(domain)?;
        return Some(Place::Mail {
            address: format!("{}@{domain}", local.to_lowercase()),
            domain,
        });
    }
    let host = url.host_str()?;
    Some(Place::Host(
        host.strip_prefix("www.").unwrap_or(host).to_string(),
    ))
}

/// The place one word of a label reads as, if it reads as one at all:
/// a scheme (`https://…`), a `www.`, an address with `@`, or a dotted name
/// ending in letters (`bank.com`, not `v1.2` or `e.g.`).
fn place_of_word(word: &str) -> Option<Place> {
    let word = word.trim_matches(|ch: char| {
        !(ch.is_alphanumeric() || matches!(ch, '/' | '-' | '_' | '@' | ':' | '%' | '~'))
    });
    let lower = word.to_lowercase();
    let lower = lower.strip_prefix("mailto:").unwrap_or(&lower);
    if lower.contains("://") {
        let url = url::Url::parse(lower).ok()?;
        return url.host_str().and_then(ascii_host).map(Place::Host);
    }
    if let Some((local, domain)) = lower.rsplit_once('@')
        && !local.is_empty()
    {
        let domain = ascii_host(dotted_name(domain)?)?;
        return Some(Place::Mail {
            address: format!("{local}@{domain}"),
            domain,
        });
    }
    let name = lower.split(['/', '?', '#']).next()?;
    let name = name.rsplit_once(':').map_or(name, |(host, port)| {
        if port.chars().all(|ch| ch.is_ascii_digit()) {
            host
        } else {
            name
        }
    });
    ascii_host(dotted_name(name)?).map(Place::Host)
}

/// `name` when it reads as a domain: two or more dotted parts, the last
/// of them letters.
fn dotted_name(name: &str) -> Option<&str> {
    let parts: Vec<&str> = name.split('.').collect();
    let last = parts.last()?;
    let letters = last.chars().count() >= 2 && last.chars().all(char::is_alphabetic)
        || last.starts_with("xn--");
    (parts.len() >= 2 && parts.iter().all(|part| !part.is_empty()) && letters).then_some(name)
}

/// The places a label's words read as.
fn label_places(text: &str) -> Vec<Place> {
    text.split_whitespace().filter_map(place_of_word).collect()
}

/// Whether a word of the label reads as an address that is not where the
/// link goes: "Sign in at paypal.com" to another host.
fn label_misleads(text: &str, href: &str) -> bool {
    let target = place_of_href(href);
    label_places(text)
        .iter()
        .any(|place| match (&target, place) {
            (None, _) => true,
            (Some(Place::Mail { address, .. }), Place::Mail { address: word, .. }) => {
                address != word
            }
            (Some(target), place) => target.domain() != place.domain(),
        })
}

/// Whether the label already names where the link goes.
fn label_names(text: &str, href: &str) -> bool {
    let places = label_places(text);
    !places.is_empty() && !label_misleads(text, href)
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

/// What a locked draft's bar offers. A draft that may have gone out can be
/// unlocked, once Sent has been checked; one that went out only closes.
fn locked_hint(lock: SendLock, wide: bool) -> &'static str {
    match (lock, wide) {
        (SendLock::MaybeSent, true) => "read-only · :unlock if not in Sent · :q close",
        (SendLock::MaybeSent, false) => ":unlock · :q close",
        (_, true) => "read-only · :q close",
        (_, false) => ":q close",
    }
}

/// The window's warning for a draft that may have gone out, at any width:
/// the person checks Sent before unlocking or sending again.
const MAYBE_SENT_WARNING: &str = "may have been sent · check Sent";

fn palette_action(query: &str) -> Option<&'static str> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return None;
    }
    // Vim's `:q`. `quote` still runs from `:qu`.
    if query == "q" || query == "quit" {
        return Some("close");
    }
    // Spelled out in full: it lets a draft that may have gone out be sent
    // again.
    if query == "unlock" {
        return Some("unlock");
    }
    let mut found = None;
    for name in [
        "send", "preview", "raw", "quote", "theme", "keep", "reject", "undo", "close",
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

/// The key as the compose window matches it. GPUI names keys ("space",
/// "bracketright", "semicolon"); the window matches what they type: " ",
/// "]", ":". Keys that type nothing keep their names ("escape", "enter").
fn compose_key(stroke: &gpui::Keystroke) -> String {
    if stroke.key == "space" {
        return " ".into();
    }
    let named = matches!(
        stroke.key.as_str(),
        "escape" | "enter" | "backspace" | "tab" | "up" | "down" | "left" | "right" | "delete"
    );
    if !named
        && !stroke.modifiers.control
        && !stroke.modifiers.platform
        && let Some(text) = stroke.key_char.as_deref()
        && text.chars().count() == 1
        && !text.chars().any(char::is_control)
    {
        return text.to_string();
    }
    stroke.key.clone()
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
    // A send is recorded in the mailbox before it starts, so the demo draft
    // gets a mailbox of its own beside it.
    let db = root.join("mail.db");
    let store = fell_core::Store::open(&db).map_err(|err| err.to_string())?;
    store
        .upsert_account(&fell_core::Account {
            id: AccountId("local".into()),
            display_name: "Jon".into(),
            email: "jon@example.com".into(),
            backend_kind: fell_core::BackendKind::Bridge,
        })
        .map_err(|err| err.to_string())?;
    drop(store);
    session.bind_store(&db);
    session.flush()?;
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
    use super::{Editor, cursor_line_covered, fit_px, plain_markdown};
    use fell_core::compose::markdown_to_doc;

    #[test]
    fn a_context_subject_gives_way_with_an_ellipsis() {
        let measure = |text: &str| text.chars().count() as f32;
        assert_eq!(fit_px(" · Plans", 20.0, &measure), " · Plans");
        assert_eq!(
            fit_px(" · Sent copies in the thread", 12.0, &measure),
            " · Sent cop…"
        );
        assert_eq!(fit_px("abc", 0.5, &measure), "");
    }

    #[test]
    fn a_maybe_sent_draft_says_check_sent_at_every_width() {
        use fell_core::compose::SendLock;
        assert!(super::MAYBE_SENT_WARNING.contains("check Sent"));
        for wide in [true, false] {
            assert!(super::locked_hint(SendLock::MaybeSent, wide).contains(":unlock"));
            assert!(super::locked_hint(SendLock::MaybeSent, wide).contains(":q close"));
            assert!(!super::locked_hint(SendLock::Sent, wide).contains(":unlock"));
        }
        // Narrow, both sides share 460 px with room to spare.
        let narrow = super::locked_hint(SendLock::MaybeSent, false)
            .chars()
            .count()
            + super::MAYBE_SENT_WARNING.chars().count();
        assert!(narrow <= 50, "{narrow}");
    }

    #[test]
    fn was_text_drops_emphasis_marks_but_not_marks_inside_words() {
        assert_eq!(
            plain_markdown("- Launch moves to *March*"),
            "Launch moves to March"
        );
        assert_eq!(
            plain_markdown("**Hiring** closes `Friday`"),
            "Hiring closes Friday"
        );
        assert_eq!(
            plain_markdown("rename snake_case_name"),
            "rename snake_case_name"
        );
        assert_eq!(plain_markdown("2*3*4 is _fine_"), "2*3*4 is fine");
    }

    #[test]
    fn compose_keys_match_what_they_type() {
        let stroke = |key: &str, char: Option<&str>| gpui::Keystroke {
            modifiers: gpui::Modifiers::default(),
            key: key.into(),
            key_char: char.map(str::to_string),
        };
        assert_eq!(super::compose_key(&stroke("space", Some(" "))), " ");
        assert_eq!(super::compose_key(&stroke("bracketright", Some("]"))), "]");
        assert_eq!(super::compose_key(&stroke("semicolon", Some(":"))), ":");
        assert_eq!(super::compose_key(&stroke("escape", None)), "escape");
        assert_eq!(super::compose_key(&stroke("enter", Some("\n"))), "enter");
        assert_eq!(super::compose_key(&stroke("h", Some("h"))), "h");
    }

    #[test]
    fn edits_are_ordered_header_first_then_by_body_line() {
        let mut snap = super::empty_snapshot("d");
        snap.body = "one\ntwo\nthree\n".into();
        snap.text = format!("---\nto: []\n---\n{}", snap.body);
        let at = |needle: &str| snap.text.find(needle).unwrap();
        let hunk = |id: u64, kind, needle: &str| fell_core::compose::HunkView {
            id,
            kind,
            start: at(needle),
            end: at(needle) + needle.len(),
            label: String::new(),
            previous: String::new(),
        };
        snap.hunks = vec![
            hunk(7, super::HunkKind::Body, "three"),
            hunk(3, super::HunkKind::Body, "one"),
            hunk(9, super::HunkKind::Recipients, "to: []"),
        ];
        assert_eq!(
            super::ordered_hunks(&snap),
            vec![(9, None), (3, Some(1)), (7, Some(3))]
        );
        assert_eq!(
            super::edit_preview(&snap, &snap.hunks[0]).as_deref(),
            Some("three")
        );
    }

    #[test]
    fn agent_names_lose_format_characters() {
        assert_eq!(super::agent_label("agent:claude-code"), "claude-code");
        assert_eq!(
            super::agent_label("agent:bot\u{202E}dneS\u{200B}"),
            "botdneS"
        );
    }

    #[test]
    fn a_reply_sender_and_subject_cannot_reorder_the_row() {
        assert_eq!(
            super::display_safe("Jon\u{202E}gnisab\u{202C}\nRe: plan"),
            "JongnisabRe: plan"
        );
    }

    #[test]
    fn compose_text_meets_aa_on_every_stock_theme() {
        use crate::theme::{MARK_CONTRAST, TEXT_CONTRAST, contrast, every_test_theme};
        let themes = every_test_theme();
        assert!(themes.len() > 10);
        for theme in themes {
            let colors = super::palette_from_theme(&theme);
            let name = &theme.name;
            for bg in [colors.bg, colors.panel, colors.code, colors.hunk] {
                assert!(
                    contrast(colors.fg, bg) >= TEXT_CONTRAST,
                    "{name} fg on {bg:06x}"
                );
            }
            for bg in [colors.bg, colors.panel, colors.hunk] {
                assert!(
                    contrast(colors.dim, bg) >= TEXT_CONTRAST,
                    "{name} dim on {bg:06x}"
                );
            }
            for bg in [colors.bg, colors.panel, colors.hunk] {
                assert!(
                    contrast(colors.accent, bg) >= TEXT_CONTRAST,
                    "{name} accent on {bg:06x}"
                );
            }
            assert!(
                contrast(colors.error, colors.bg) >= TEXT_CONTRAST,
                "{name} error"
            );
            assert!(
                contrast(colors.marker, colors.bg) >= MARK_CONTRAST,
                "{name} marker"
            );
            // Markers stay quieter than the prose they mark.
            assert!(
                contrast(colors.marker, colors.bg) < contrast(colors.fg, colors.bg),
                "{name}"
            );
        }
    }

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
        assert_eq!(super::palette_action("q"), Some("close"));
        assert_eq!(super::palette_action("quit"), Some("close"));
        assert_eq!(super::palette_action("qu"), Some("quote"));
        assert_eq!(super::palette_action("cl"), Some("close"));
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
    fn tab_from_subject_keeps_typing_in_the_body() {
        // #157: the body was left in normal mode, so "# Heading" typed
        // after Tab ran as commands and left "ding".
        let mut editor = super::Editor::from_body("");
        assert!(editor.mode == super::Mode::Normal);
        let next = super::leave_header(super::Field::Subject, &mut editor);
        assert!(next == super::Field::Body);
        assert!(editor.mode == super::Mode::Insert, "typing goes on as text");
        assert_eq!((editor.row, editor.col), (0, 0));
    }

    #[test]
    fn tab_between_header_fields_leaves_the_body_alone() {
        let mut editor = super::Editor::from_body("Hi Ada,\n");
        for (from, to) in [
            (super::Field::To, super::Field::Cc),
            (super::Field::Cc, super::Field::Bcc),
            (super::Field::Bcc, super::Field::Subject),
        ] {
            assert!(super::leave_header(from, &mut editor) == to);
            assert!(editor.mode == super::Mode::Normal);
        }
    }

    #[test]
    fn a_draft_started_on_its_header_resumes_after_its_text() {
        let mut editor = super::Editor::from_body("Hi Ada,\n\nThanks for the notes\n\n");
        editor.cursor_to_end_of_text();
        assert_eq!((editor.row, editor.col), (2, "Thanks for the notes".len()));
        let mut empty = super::Editor::from_body("\n\n");
        empty.row = 1;
        empty.cursor_to_end_of_text();
        assert_eq!((empty.row, empty.col), (0, 0));
    }

    /// The text a source line shows with these fold settings, a link's
    /// note after it with a space, as `raw_line` paints them.
    fn shown(line: &str, show_markers: bool, urls: super::Urls) -> String {
        let mut out = String::new();
        let mut label = "";
        for span in fell_core::compose::line_styles(line) {
            let slice = &line[span.start..span.end];
            if span.kind == super::StyleKind::LinkText {
                label = slice;
            }
            if !super::folds(span.kind, show_markers, urls) {
                out.push_str(slice);
            }
            if span.kind == super::StyleKind::LinkUrl
                && let Some(note) = super::url_note(label, slice, urls)
            {
                out.push(' ');
                out.push_str(&note);
            }
        }
        out
    }

    #[test]
    fn a_link_url_folds_away_off_the_cursor_line() {
        use super::{Links, Urls};
        // #158: the URL (and its "]") stayed on every line.
        let line = "See [the plan](https://example.com/plan) for dates.";
        assert_eq!(
            shown(line, false, Urls::Fold(Links::Label)),
            "See the plan for dates."
        );
        // The cursor line shows the source, dimmed markers and all.
        assert_eq!(shown(line, true, Urls::Source), line);
        // A line an agent changed keeps its destination in view.
        assert_eq!(
            shown(line, false, Urls::Source),
            "See the plan](https://example.com/plan) for dates."
        );
    }

    #[test]
    fn agent_words_show_where_each_link_goes() {
        use super::{Links, Urls, shown_target};
        // An agent's create_draft body has no hunks: every link in it
        // says where it goes, styled and on a raw line alike.
        let label = "pay the invoice";
        let href = "https://evil.example/login";
        assert_eq!(shown_target(label, href, Links::Label), None);
        assert_eq!(
            shown_target(label, href, Links::Host).as_deref(),
            Some("(evil.example)")
        );
        let line = "Please [pay the invoice](https://evil.example/login) today.";
        assert_eq!(
            shown(line, false, Urls::Fold(Links::Host)),
            "Please pay the invoice (evil.example) today."
        );
        // A label that already names the host says it once.
        assert_eq!(
            shown_target("evil.example", "https://www.evil.example/", Links::Host),
            None
        );
        assert_eq!(
            shown_target("write to ada", "mailto:ada@example.com", Links::Host).as_deref(),
            Some("(ada@example.com)")
        );
    }

    #[test]
    fn a_label_that_reads_as_an_address_shows_where_it_goes() {
        use super::{Links, Urls, shown_target};
        assert_eq!(
            shown_target("the plan", "https://example.com/plan", Links::Label),
            None
        );
        assert_eq!(
            shown_target("example.com", "https://www.example.com/", Links::Label),
            None
        );
        assert_eq!(
            shown_target("bank.example", "https://other.example/login", Links::Label).as_deref(),
            Some("(other.example)")
        );
        // Any word of the label, not only a one-word label.
        assert_eq!(
            shown_target(
                "Sign in at paypal.com",
                "https://evil.example/login",
                Links::Label
            )
            .as_deref(),
            Some("(evil.example)")
        );
        assert_eq!(
            shown_target(
                "Sign in at paypal.com.",
                "https://www.paypal.com/signin",
                Links::Label
            ),
            None
        );
        // A host that only starts like the label's is somewhere else.
        assert_eq!(
            shown_target("bank.com", "https://bank.com.evil.example/", Links::Label).as_deref(),
            Some("(bank.com.evil.example)")
        );
        assert_eq!(
            shown_target("ada@example.com", "mailto:ada@example.com", Links::Label),
            None
        );
        assert_eq!(
            shown_target("ada@example.com", "mailto:eve@example.com", Links::Label).as_deref(),
            Some("(eve@example.com)")
        );
        // Words with dots that are not addresses.
        for label in ["v1.2 notes", "see e.g. the plan", "3.5 stars"] {
            assert_eq!(
                shown_target(label, "https://example.com/", Links::Label),
                None,
                "{label}"
            );
        }
        // On a raw line off the cursor, the same check runs.
        let line = "[bank.com](https://evil.example/login) and more";
        assert_eq!(
            shown(line, false, Urls::Fold(Links::Label)),
            "bank.com (evil.example) and more"
        );
        assert_eq!(
            shown(
                "[**bank.com**](https://evil.example/)",
                false,
                Urls::Fold(Links::Label)
            ),
            "**bank.com** (evil.example)"
        );
        // The HTML preview before send shows every destination.
        assert_eq!(
            shown_target("the plan", "https://example.com/plan", Links::Target).as_deref(),
            Some("(https://example.com/plan)")
        );
    }

    #[test]
    fn an_idn_host_shows_in_its_ascii_form() {
        use super::{Links, Urls, shown_target};
        // Cyrillic letters that read as "paypal".
        let href = "https://\u{440}\u{430}\u{443}\u{440}\u{430}l.com/login";
        let note = |links| shown_target("the account", href, links).unwrap();
        for links in [Links::Label, Links::Host] {
            let note = note(links);
            assert!(
                note.starts_with("(xn--") && note.ends_with(".com)"),
                "{note}"
            );
        }
        assert!(note(Links::Target).starts_with("(https://xn--"));
        // A label that is the URL still shows where it really goes.
        assert!(shown_target(href, href, Links::Target).is_some());
        // As written, on the cursor line, the ASCII form sits beside it.
        let line = format!("[the account]({href})");
        let raw = shown(&line, true, Urls::Source);
        assert!(raw.starts_with(&line) && raw.contains("(xn--"), "{raw}");
    }

    #[test]
    fn a_hunk_is_found_where_it_is_not_where_its_text_first_appears() {
        // The same link earlier in the body is not the agent's edit.
        let link = "[pay](https://example.com/pay)";
        let mut snap = super::empty_snapshot("d");
        snap.body = format!("{link}\n\nThanks,\n{link}\n");
        snap.text = format!("---\nto: []\n---\n{}", snap.body);
        let base = snap.text.len() - snap.body.len();
        let second = snap.body.rfind(link).unwrap();
        snap.hunks = vec![fell_core::compose::HunkView {
            id: 4,
            kind: super::HunkKind::Body,
            start: base + second,
            end: base + second + link.len() + 1,
            label: String::new(),
            previous: "old".into(),
        }];
        assert_eq!(
            super::body_edit_ranges(&snap),
            vec![(second, second + link.len(), "old".to_string(), 4)]
        );
        assert_eq!(super::ordered_hunks(&snap), vec![(4, Some(4))]);
    }

    #[test]
    fn the_status_bar_says_the_mode_and_nothing_obvious() {
        for status in ["", "insert", "normal"] {
            assert!(super::idle_status(status), "{status}");
        }
        assert!(!super::idle_status("Sending…"));
    }

    #[test]
    fn a_missing_recipient_blocks_send_only_once_send_is_tried() {
        let mut snap = super::empty_snapshot("d");
        snap.send_block = Some(super::NO_RECIPIENT.into());
        assert_eq!(super::shown_send_block(&snap, false), None);
        assert_eq!(
            super::shown_send_block(&snap, true).map(String::as_str),
            Some(super::NO_RECIPIENT)
        );
        // Any other block shows at once.
        snap.send_block = Some("from is not this account".into());
        assert!(super::shown_send_block(&snap, false).is_some());
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

    #[test]
    fn the_cursor_follows_its_line_when_lines_appear_above() {
        let mut editor = Editor::from_body("one\ntwo\nthree\n");
        editor.row = 1;
        editor.col = 2;
        let line = editor.lines[editor.row].clone();
        let mut moved = Editor::from_body("agent\nadded\none\ntwo\nthree\n");
        moved.row = editor.row;
        moved.col = editor.col;
        assert!(moved.follow_line(&line));
        assert_eq!(moved.lines[moved.row], "two");
        // The line is gone, or now appears twice: do not guess.
        let mut gone = Editor::from_body("one\nthree\n");
        gone.row = 1;
        assert!(!gone.follow_line("two"));
        let mut twice = Editor::from_body("two\nx\ntwo\n");
        twice.row = 1;
        assert!(!twice.follow_line("two"));
    }
}
