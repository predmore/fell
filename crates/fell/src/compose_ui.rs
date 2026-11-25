//! Compose window. The buffer is plain markdown. Styling is a view of that
//! text, through the same document model the conversation renderer uses.

mod recipients;
pub(crate) use recipients::RecipientStore;

use std::collections::{HashMap, HashSet};
use std::net::Ipv6Addr;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use fell_core::compose::{
    BlockBody, DraftHub, DraftSaveStatus, HunkKind, SaveReceipt, SendLock, Snapshot, StyleKind,
    StyleSpan, StyledDoc, html_to_doc, line_styles, markdown_to_doc,
};
use fell_core::{AccountId, MailBackend};
use gpui::{
    Context, FocusHandle, FontWeight, KeyDownEvent, RenderImage, SharedString,
    StatefulInteractiveElement, Window, deferred, div, img, prelude::*, px, rgb, rgba,
};
use image::Frame;
use smallvec::SmallVec;

use crate::confirmation_from_ui;
use crate::mailbox::AccountConfigError;
use crate::motion::{self, Motion, Transition};
use crate::present::display_safe;
use crate::reply_check::{ReplyCheck, ReplyChecks};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ThemeKind {
    Dark,
    Light,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Palette {
    bg: u32,
    fg: u32,
    dim: u32,
    /// Quieter text that still reads at AA on the page and panel: where a
    /// link goes, beside its label.
    muted: u32,
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
            muted: 0x9a938c,
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
            muted: 0x655e57,
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
        muted: ensure_all(theme.dimmed, &[bg, panel, code], TEXT_CONTRAST),
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum SendPanel {
    Summary,
    Details,
    ConfirmUnlock,
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
    recipients: recipients::Completion,
    styled_cache: Option<(StyledBodyKey, gpui::Entity<CachedStyledBody>)>,
    frames: Option<crate::frame_log::SharedFrameLog>,
    theme: ThemeKind,
    /// The Omarchy theme this window follows, live. `None` uses the
    /// built-in palette for `theme`.
    look: Option<crate::theme::Theme>,
    ui_px: f32,
    raw: bool,
    preview: bool,
    quote_open: bool,
    quote_scroll: gpui::ScrollHandle,
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
    draft_save: Option<(u64, DraftSaveStatus, Option<Transition>)>,
    draft_save_exit: Option<Transition>,
    announced_shown: Option<SharedString>,
    motion: Motion,
    /// `:q` asked to close this window. The next frame removes it.
    closing: bool,
    discarding: bool,
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
    send_feedback: SendFeedbackState,
    send_check: Option<gpui::Task<()>>,
    send_panel: SendPanel,
    send_detail_scroll: gpui::ScrollHandle,
}

#[derive(PartialEq)]
struct StyledBodyKey {
    draft_id: SharedString,
    revision: u64,
    hunk: Option<u64>,
    colors: Palette,
    family: String,
    text_px: f32,
}

struct CachedStyledBody {
    compose: gpui::WeakEntity<ComposeView>,
    colors: Palette,
    family: String,
}

impl Render for CachedStyledBody {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.compose
            .update(cx, |compose, cx| {
                let snap = compose
                    .hub
                    .with_session(compose.draft_id.as_ref(), |session| session.snapshot());
                snap.map(|snap| {
                    compose
                        .body_area(&snap, self.colors, window, &self.family, cx)
                        .into_any_element()
                })
                .unwrap_or_else(|| div().into_any_element())
            })
            .unwrap_or_else(|_| div().into_any_element())
    }
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
        cx.default_global::<SendAttempts>();
        cx.default_global::<ReplyChecks>();
        cx.observe_global::<ReplyChecks>(|_, cx| cx.notify())
            .detach();
        cx.observe_global::<SendAttempts>(|this, cx| {
            this.send_feedback.invalidate_readiness();
            cx.notify();
        })
        .detach();
        // A closed window no longer counts as showing its draft.
        cx.on_release(|this: &mut Self, cx| {
            this.hub.note_hidden(this.draft_id.as_ref());
            if !this.closing {
                crate::view::compose_draft_closing(this.draft_id.as_ref(), cx);
            }
        })
        .detach();
        let mut view = Self {
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
            recipients: recipients::Completion::default(),
            styled_cache: None,
            frames: crate::frame_log::FrameLog::from_env(),
            theme: ThemeKind::Dark,
            look: None,
            ui_px: 16.,
            raw: false,
            preview: false,
            quote_open: false,
            quote_scroll: gpui::ScrollHandle::new(),
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
            draft_save: None,
            draft_save_exit: None,
            announced_shown: None,
            motion: Motion::from_env(),
            closing: false,
            discarding: false,
            close_on_send: false,
            bracket: None,
            focused_hunk: None,
            seen_edits: 0,
            focus_by_user: false,
            touched: false,
            known_hunks: Vec::new(),
            send_tried: false,
            send_feedback: SendFeedbackState::default(),
            send_check: None,
            send_panel: SendPanel::Summary,
            send_detail_scroll: gpui::ScrollHandle::new(),
        };
        view.refresh_send_availability(cx);
        view
    }

    /// Check the small local account file while this draft is open. Polling
    /// also sees file replacements, permission changes, and a newly created
    /// config directory. An attempted send reads it again independently.
    fn refresh_send_availability(&mut self, cx: &mut Context<Self>) {
        let Some(account) = self.hub.with_session(self.draft_id.as_ref(), |session| {
            session.account_id().clone()
        }) else {
            return;
        };
        // Replacing the task on activation or a draft switch drops any
        // outstanding result before it can repaint the window.
        self.send_check = Some(cx.spawn(async move |this, cx| {
            if fell_core::is_demo_account(&account) {
                return;
            }
            loop {
                let Ok(generation) = this.update(cx, |this, _| this.send_feedback.generation)
                else {
                    break;
                };
                let account = account.clone();
                let unavailable = cx
                    .background_executor()
                    .spawn(async move { crate::mailbox::send_account_config(&account).err() })
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if this.send_feedback.update_readiness(generation, unavailable) {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
            }
        }));
    }

    pub fn watch_live(&self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let Ok(Some((id, account, revision, receipt, store))) =
                    this.update(cx, |this, cx| {
                        let id = this.draft_id.clone();
                        if matches!(this.field, Field::Subject | Field::Body)
                            || fell_core::compose::parse_address_line(this.header_text()).is_ok()
                        {
                            this.hub.poll_server_draft(id.as_ref(), now_ms());
                        }
                        let (account, revision, receipt) =
                            this.hub.with_session(id.as_ref(), |session| {
                                Some((
                                    session.account_id().clone(),
                                    session.revision(),
                                    session.save_receipt()?,
                                ))
                            })??;
                        if this.draft_save.is_some_and(|(saved, status, _)| {
                            saved == revision && status.warning().is_none()
                        }) {
                            return None;
                        }
                        let store = cx
                            .try_global::<RecipientStore>()
                            .map(|store| store.0.clone());
                        Some((id, account, revision, receipt, store))
                    })
                else {
                    if this.upgrade().is_none() {
                        break;
                    }
                    continue;
                };
                let status = match receipt {
                    SaveReceipt::Local => Some(DraftSaveStatus::Local),
                    SaveReceipt::Pending => None,
                    SaveReceipt::ServerFailed => Some(DraftSaveStatus::ServerFailed),
                    SaveReceipt::Server(digest) => {
                        let Some(store) = store else { continue };
                        let draft = id.clone();
                        store
                            .call_background(move |store| {
                                store.server_draft_save_status(&account, draft.as_ref(), &digest)
                            })
                            .await
                            .ok()
                            .and_then(Result::ok)
                            .flatten()
                    }
                };
                if let Some(status) = status {
                    let _ = this.update(cx, |this, cx| {
                        if this.draft_id == id
                            && this
                                .hub
                                .with_session(id.as_ref(), |session| session.revision())
                                == Some(revision)
                            && !this.draft_save.is_some_and(|(saved, previous, _)| {
                                (saved, previous) == (revision, status)
                            })
                        {
                            let fade = this.motion.start(motion::FADE, Instant::now());
                            this.draft_save = Some((revision, status, fade));
                            this.draft_save_exit = None;
                            cx.notify();
                        }
                    });
                }
            }
        })
        .detach();
        cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() && !this.closing {
                crate::view::compose_draft_shown(&this.draft_id, cx);
                this.refresh_send_availability(cx);
            }
        })
        .detach();
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
        self.set_font_size(theme.ui_px);
        self.look = Some(theme);
    }

    pub(crate) fn set_font_size(&mut self, next: f32) -> bool {
        if self.ui_px == next {
            return false;
        }
        self.ui_px = next;
        true
    }

    pub(crate) fn set_motion(&mut self, next: Motion) -> bool {
        if self.motion.enabled() == next.enabled() {
            return false;
        }
        self.motion = next;
        true
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
        // The edit bar says which edit this is.
        self.status = "".into();
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
        // The bar shows the edit now, so the status line says only what
        // happened.
        let what = match (
            new.len(),
            order_hunk(&snap, &order, at).map(|hunk| hunk.kind),
        ) {
            (1, Some(HunkKind::Recipients)) => "changed the recipients".to_string(),
            (1, Some(HunkKind::Subject)) => "changed the subject".to_string(),
            (1, Some(HunkKind::Attachments)) => "changed the attachments".to_string(),
            (1, _) => "made an edit".to_string(),
            (count, _) => format!("made {count} edits"),
        };
        self.status = format!("{who} {what}").into();
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
        if self
            .hub
            .with_session(self.draft_id.as_ref(), |_| ())
            .is_none()
        {
            self.closing = true;
            return;
        }
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
        self.draft_save = None;
        self.draft_save_exit = None;
        self.discarding = false;
        self.editor = Editor::from_body(&snap.body);
        self.applied_revision = snap.revision;
        self.field = Field::Body;
        self.to_line = join_addrs(&snap.to);
        self.cc_line = join_addrs(&snap.cc);
        self.bcc_line = join_addrs(&snap.bcc);
        self.subject_line = snap.subject.clone();
        self.sent = false;
        self.send_tried = false;
        self.send_feedback.switch_draft();
        self.send_check = None;
        self.send_panel = SendPanel::Summary;
        self.palette = None;
        self.leader = false;
        self.which_visible = false;
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
        self.set_header_row(field, &snap);
    }

    fn set_header_row(&mut self, field: Field, snap: &Snapshot) {
        match field {
            Field::To => self.to_line = join_addrs(&snap.to),
            Field::Cc => self.cc_line = join_addrs(&snap.cc),
            Field::Bcc => self.bcc_line = join_addrs(&snap.bcc),
            Field::Subject => self.subject_line = snap.subject.clone(),
            Field::Body => {}
        }
    }

    fn record_field(&self, field: Field) -> Option<Result<(), String>> {
        self.hub
            .with_session(self.draft_id.as_ref(), |session| match field {
                Field::To => session.set_addresses_from_user("to", &self.to_line),
                Field::Cc => session.set_addresses_from_user("cc", &self.cc_line),
                Field::Bcc => session.set_addresses_from_user("bcc", &self.bcc_line),
                Field::Subject => {
                    session.set_subject_from_user(self.subject_line.clone());
                    Ok(())
                }
                Field::Body => Ok(()),
            })
    }

    fn commit_field(&mut self, field: Field) -> bool {
        let id = self.draft_id.to_string();
        let result = self.record_field(field);
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
    /// on a background thread. The window keeps painting while the server
    /// answers. The draft is claimed for the send before it starts, so a
    /// second press, here or in a reopened window, never sends it again.
    fn send(&mut self, cx: &mut Context<Self>) {
        let id = self.draft_id.to_string();
        self.send_tried = true;
        if self.sending.as_deref() == Some(id.as_str()) {
            return;
        }
        if self
            .hub
            .with_session(&id, |session| session.send_lock().is_some())
            == Some(true)
        {
            return;
        }
        self.send_feedback.invalidate_readiness();
        cx.global_mut::<SendAttempts>().clear(&id);
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
            Some(Err(SendFailure::Locked(_))) => {
                self.status = "".into();
                return;
            }
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
        let attempt = cx.global_mut::<SendAttempts>().start(&id);
        self.preview = false;
        self.status = "".into();
        let hub = self.hub.clone();
        cx.spawn(async move |this, cx| {
            let sent_id = id.clone();
            let (outcome, copy) = cx
                .background_executor()
                .spawn({
                    let hub = hub.clone();
                    async move { transmit_from_hub(&hub, &id, prepared, account_backend) }
                })
                .await;
            let copy_saved = copy.as_ref().map(Result::is_ok);
            if let Some(Ok((thread, message, rich))) = copy {
                let _ = cx.update(|cx| crate::view::present_sent_copy(cx, thread, message, rich));
            }
            let current = cx
                .update_global::<SendAttempts, _>(|attempts, _| {
                    attempts.finish(&sent_id, attempt, outcome.as_ref().err().cloned())
                })
                .unwrap_or(false);
            if current {
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
                        if !end.close {
                            this.status = match &outcome {
                                Err(SendFailure::Locked(_)) | Ok(_) => status.into(),
                                Err(_) => "".into(),
                            };
                        }
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
            }
            if let Some(saved) = copy_saved {
                if !saved {
                    hub.report_sent_copy_failure(&sent_id);
                }
                let _ = cx.update(crate::view::wake_sent_copy_sync);
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
            let snap = self.hub.with_session(&id, |session| session.snapshot());
            // Ordinary sync preserves the active line while it is being typed.
            // Undo replaces it too. Header carets are drawn at the line's end.
            if let Some(snap) = &snap {
                self.set_header_row(self.field, snap);
            }
            self.sync_from_hub();
            // An edit that came back (a keep or reject undone) takes focus.
            let returned = snap.as_ref().and_then(|snap| {
                let order = ordered_hunks(snap);
                let at = order.iter().position(|(hunk, _)| !before.contains(hunk))?;
                let name = edit_name(order_hunk(snap, &order, at)?);
                Some((order, at, name))
            });
            match returned {
                Some((order, at, name)) => {
                    self.focus_hunk_at(&order, at);
                    self.status = format!("{name} back for review").into();
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
            .map(edit_name)
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

    fn place_link_cursor(&mut self, row: usize, line: &str, col: usize, cx: &mut Context<Self>) {
        if self.field != Field::Body
            || self.preview
            || self.palette.is_some()
            || self.leader
            || self.editor.row != row
            || !self.pull_if_stale()
            || self.editor.lines.get(self.editor.row).map(String::as_str) != Some(line)
        {
            return;
        }
        self.editor.col = col.min(line.chars().count());
        self.editor.preferred = self.editor.col;
        self.editor.count = 0;
        self.bracket = None;
        self.touched = true;
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if self.discarding || self.closing {
            return;
        }
        self.touched = true;
        let key = compose_key(&event.keystroke);
        let key = key.as_str();
        if self.send_panel != SendPanel::Summary {
            self.send_panel_key(key, &event.keystroke.modifiers, cx);
            return;
        }
        if self.field == Field::Body && !self.pull_if_stale() {
            self.status = "the draft changed under the cursor; the key was not applied".into();
            return;
        }
        // A draft being sent, or already sent, is read-only: what is on
        // screen must stay what went out.
        let id = self.draft_id.to_string();
        let palette_shortcut =
            key == "p" && event.keystroke.modifiers.control && event.keystroke.modifiers.shift;
        let typed =
            event.keystroke.key_char.as_deref().filter(|_| {
                !event.keystroke.modifiers.control && !event.keystroke.modifiers.platform
            });
        if let Some(Some(lock)) = self.hub.with_session(&id, |session| session.send_lock())
            && key != "escape"
        {
            // Still a window: `:` and its commands work, so `:q` closes
            // it; `run_action` refuses the ones that would change the draft.
            if self.palette.is_some() {
                self.palette_key(key, typed, cx);
            } else if key == ":" || palette_shortcut {
                self.palette = Some(String::new());
            } else if self.leader {
                self.leader_key(key, cx);
            } else if key == " " && lock == SendLock::MaybeSent {
                self.begin_leader();
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
                if self.dismiss_recipient_menu() {
                    return;
                }
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
            if palette_shortcut {
                self.palette = Some(String::new());
            } else if !self.recipient_key(key, &event.keystroke.modifiers) {
                self.field_key(key, typed);
            }
            self.arm_debounced_flush(cx);
            return;
        }
        if event.keystroke.modifiers.control && key == "enter" {
            return;
        }
        match self.editor.mode {
            Mode::Insert => self.insert_key(key, typed),
            Mode::Normal if palette_shortcut => self.palette = Some(String::new()),
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
            "unlock" if locked == Some(SendLock::MaybeSent) => {
                self.status = "".into();
                self.send_panel = SendPanel::ConfirmUnlock;
            }
            "details"
                if self
                    .send_feedback
                    .feedback(
                        locked,
                        cx.global::<SendAttempts>().failure(self.draft_id.as_ref()),
                    )
                    .is_some_and(|feedback| feedback.detail().is_some()) =>
            {
                self.send_detail_scroll = gpui::ScrollHandle::new();
                self.send_panel = SendPanel::Details;
            }
            "dismiss" if locked.is_none() => {
                cx.global_mut::<SendAttempts>()
                    .clear(self.draft_id.as_ref());
            }
            "send" => self.send(cx),
            "complete recipient" if locked.is_none() => {
                self.accept_recipient(self.recipients.selected());
            }
            "discard" => self.discard(cx),
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
                crate::view::compose_draft_closing(self.draft_id.as_ref(), cx);
                cx.notify();
            }
            "diagnostic log" => {
                self.status = "".into();
                crate::view::open_compose_diagnostic_log(cx);
            }
            _ => {}
        }
    }

    fn discard(&mut self, cx: &mut Context<Self>) {
        let id = self.draft_id.to_string();
        let hub = self.hub.clone();
        self.discarding = true;
        let discard = cx.background_spawn({
            let id = id.clone();
            async move { hub.discard_open(&id) }
        });
        cx.spawn(async move |this, cx| {
            let result = discard.await;
            let _ = this.update(cx, |view, cx| {
                if view.draft_id.as_ref() == id {
                    view.discarding = false;
                    match result {
                        Ok(()) => view.closing = true,
                        Err(err) => view.status = err.to_string().into(),
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn send_panel_key(&mut self, key: &str, modifiers: &gpui::Modifiers, cx: &mut Context<Self>) {
        if key == "escape" {
            self.send_panel = SendPanel::Summary;
            return;
        }
        match self.send_panel {
            SendPanel::ConfirmUnlock if key == "y" && !modifiers.modified() => {
                self.send_panel = SendPanel::Summary;
                let result = self.hub.with_session(self.draft_id.as_ref(), |session| {
                    session.release_maybe_sent()
                });
                self.status = match result {
                    Some(Ok(())) => {
                        cx.global_mut::<SendAttempts>()
                            .clear(self.draft_id.as_ref());
                        "Draft unlocked".into()
                    }
                    Some(Err(err)) => err.into(),
                    None => "draft is not open".into(),
                };
                cx.notify();
            }
            SendPanel::Details if matches!(key, "j" | "k" | "down" | "up") => {
                let delta = if matches!(key, "j" | "down") {
                    -42.
                } else {
                    42.
                };
                let mut offset = self.send_detail_scroll.offset();
                offset.y = (offset.y + px(delta))
                    .clamp(-self.send_detail_scroll.max_offset().height, px(0.));
                self.send_detail_scroll.set_offset(offset);
            }
            _ => {}
        }
    }

    fn leader_key(&mut self, key: &str, cx: &mut Context<Self>) {
        match key {
            "m" => {
                self.status =
                    "mail: d discard · s send · p preview · r raw · q quote · t theme".into();
                return;
            }
            "a" => {
                self.status = "agent: k keep · x reject".into();
                return;
            }
            "?" => {
                self.status = "help: l diagnostic log".into();
                return;
            }
            "d" | "s" | "p" | "r" | "q" | "t" | "k" | "x" | "l" => {}
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
                "d" => "discard",
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
        } else if status.starts_with("help:") && key == "l" {
            "diagnostic log"
        } else {
            ""
        };
        self.leader = false;
        self.which_visible = false;
        if !action.is_empty() {
            self.run_action(action, cx);
        }
    }

    fn begin_leader(&mut self) {
        self.leader = true;
        self.leader_at = Some(std::time::Instant::now());
        self.which_visible = false;
        self.status = "space".into();
    }

    fn header_matches(&self, snap: &Snapshot) -> bool {
        let addresses = match self.field {
            Field::Body => return true,
            Field::Subject => return self.subject_line == snap.subject,
            Field::To => &snap.to,
            Field::Cc => &snap.cc,
            Field::Bcc => &snap.bcc,
        };
        fell_core::compose::parse_address_line(self.header_text()).is_ok_and(|line| {
            line.iter()
                .map(String::as_str)
                .eq(addresses.iter().map(|address| address.raw.as_str()))
        })
    }

    fn header_text(&self) -> &str {
        match self.field {
            Field::To => &self.to_line,
            Field::Cc => &self.cc_line,
            Field::Bcc => &self.bcc_line,
            Field::Subject => &self.subject_line,
            Field::Body => "",
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
            "tab" | "enter" => {
                let field = self.field;
                if self.commit_field(field) {
                    self.field = leave_header(field, &mut self.editor);
                    if self.field == Field::Body {
                        self.status = "insert".into();
                    }
                }
                return;
            }
            "backspace" => {
                line.pop();
            }
            _ => {
                let Some(ch) = typed else { return };
                line.push_str(ch);
            }
        }
        // Recipient completion owns unfinished tokens. Only committed
        // recipients are eligible for a server save.
        if self.field == Field::Subject && self.record_field(self.field) == Some(Ok(())) {
            self.hub.with_session(self.draft_id.as_ref(), |session| {
                session.arm_outbound(now_ms())
            });
            self.flush_pending = true;
        }
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
            " " => self.begin_leader(),
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
        if let Some(frames) = &self.frames {
            frames
                .borrow_mut()
                .render_started(std::time::Instant::now(), self.recipients.animating());
        }
        self.sync_from_hub();
        self.update_recipient_completion(cx);
        if self.closing {
            window.remove_window();
        }
        let attempt = cx
            .global::<SendAttempts>()
            .failure(self.draft_id.as_ref())
            .cloned();
        if self.send_check.is_none() {
            self.refresh_send_availability(cx);
        }
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
        let (mut snap, mut lock, reply_check) = self
            .hub
            .with_session(self.draft_id.as_ref(), |session| {
                let snap = session.snapshot();
                let check = snap.reply_to_unverified.then(|| {
                    if snap
                        .reply_context
                        .as_ref()
                        .is_some_and(|parent| parent.reply_to.is_some())
                    {
                        ReplyCheck::Verified
                    } else if let Some(parent) = snap.in_reply_to.as_deref() {
                        cx.global::<ReplyChecks>()
                            .for_draft(session.account_id(), parent)
                    } else {
                        ReplyCheck::Waiting
                    }
                });
                (snap, session.send_lock(), check)
            })
            .unwrap_or_else(|| (empty_snapshot(self.draft_id.as_ref()), None, None));
        // The inbox gets a chance to lay out the reply before we close.
        // Keep the in-flight footer until it takes over.
        if self.close_on_send
            && (self.closing || self.sending.as_deref() == Some(self.draft_id.as_ref()))
            && lock == Some(SendLock::Sent)
        {
            lock = Some(SendLock::InFlight);
            snap.send_block = Some(SendLock::InFlight.reason().into());
        }
        if self.send_feedback.readiness.is_some() && lock.is_none() {
            snap.send_block = Some(SEND_UNAVAILABLE.into());
        }
        let colors = self.colors();
        let ui_family = self
            .look
            .as_ref()
            .and_then(|theme| theme.ui_family.clone())
            .unwrap_or_else(|| "Liberation Sans".into());
        let wide = window.viewport_size().width > px(640.);
        let recipient_theme = self.look.as_ref().unwrap_or_else(|| {
            crate::theme::builtin_for_compose(matches!(self.theme, ThemeKind::Dark))
        });
        let overlay = self.recipient_overlay(window, cx, recipient_theme);
        let feedback = self.send_feedback.feedback(lock, attempt.as_ref());
        if (self.send_panel == SendPanel::ConfirmUnlock && lock != Some(SendLock::MaybeSent))
            || (self.send_panel == SendPanel::Details
                && feedback
                    .as_ref()
                    .is_none_or(|feedback| feedback.detail().is_none()))
        {
            self.send_panel = SendPanel::Summary;
        }
        self.editor.caret_hidden =
            self.field != Field::Body || lock.is_some() || self.send_panel != SendPanel::Summary;
        let show_status = self.send_panel == SendPanel::Summary
            && (!matches!(lock, Some(SendLock::MaybeSent | SendLock::Sent))
                || !idle_status(&self.status));
        let palette_help = match (
            lock,
            feedback
                .as_ref()
                .is_some_and(|feedback| feedback.detail().is_some()),
        ) {
            (Some(SendLock::MaybeSent), true) => "details · unlock · q close",
            (Some(SendLock::MaybeSent), false) => "unlock · q close",
            (Some(_), _) => "q close",
            (None, true) if attempt.is_some() => "details · dismiss · send · q close",
            (None, true) => {
                "details · send · preview · raw · quote · theme · keep · reject · undo · diagnostic log"
            }
            (None, false) => {
                "send · preview · raw · quote · theme · keep · reject · undo · diagnostic log"
            }
        };
        let mode = match self.editor.mode {
            // A locked draft has no editing mode.
            _ if matches!(lock, Some(SendLock::MaybeSent | SendLock::Sent)) => "",
            // A header field takes typing as insert mode does.
            _ if self.field != Field::Body => "insert",
            Mode::Normal => "normal",
            Mode::Insert => "insert",
            Mode::Visual => "visual",
        };
        // The mode word says it all; how to change mode is not news.
        let hint: SharedString = if idle_status(&self.status) {
            "".into()
        } else {
            self.status.clone()
        };
        let header_matches = self.header_matches(&snap);
        let save_exit = &mut self.draft_save_exit;
        let motion = self.motion;
        let draft_save = self.draft_save.and_then(|(revision, status, fade)| {
            if lock.is_some() {
                return None;
            }
            let now = Instant::now();
            if revision == snap.revision && header_matches {
                *save_exit = None;
                let opacity = fade.map_or(1., |fade| {
                    if fade.running(now) {
                        window.request_animation_frame();
                    }
                    fade.progress(now)
                });
                return Some((status, opacity));
            }
            if status.warning().is_some() {
                return Some((DraftSaveStatus::ServerRetrying, 1.));
            }
            if !motion.enabled() {
                return None;
            }
            let exit = save_exit.get_or_insert_with(|| Transition::new(now, motion::FADE));
            if exit.running(now) {
                window.request_animation_frame();
                return Some((status, exit.remaining(now)));
            }
            None
        });
        if !self.closing && self.announced_shown.as_ref() != Some(&self.draft_id) {
            let draft = self.draft_id.clone();
            self.announced_shown = Some(draft.clone());
            window.on_next_frame(move |_, cx| crate::view::compose_draft_shown(&draft, cx));
        }
        let mono = self
            .look
            .as_ref()
            .and_then(|theme| theme.mono_family.clone());
        let feedback = feedback.map(|feedback| {
            send_feedback_view(feedback, self.send_panel, &self.send_detail_scroll, colors)
        });
        let body = self.body_view(&snap, colors, window, &ui_family, cx);
        div()
            .id("compose")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if let Some(frames) = &this.frames {
                    frames
                        .borrow_mut()
                        .key(std::time::Instant::now(), event.keystroke.unparse());
                }
                this.on_key(event, cx);
                this.update_recipient_completion(cx);
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
            .text_size(px(self.ui_px))
            .font_weight(FontWeight(400.))
            .font_family(ui_family.clone())
            .when_some(self.frames.clone(), |root, frames| {
                let paint = frames.clone();
                root.child(
                    gpui::canvas(
                        move |_, _, _| frames.borrow_mut().mark(0, std::time::Instant::now()),
                        move |_, _, _, _| paint.borrow_mut().mark(2, std::time::Instant::now()),
                    )
                    .absolute()
                    .size_0(),
                )
            })
            .child(header(&snap, colors).flex_shrink_0())
            // Everything between the header and the Attachments divider:
            // the HTML preview panel covers exactly this, so it never ends
            // partway into a row below.
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .child(
                        self.fields_view(
                            &snap,
                            colors,
                            wide,
                            window,
                            &ui_family,
                            reply_check,
                            overlay,
                        )
                        .flex_shrink_0(),
                    )
                    .child(body)
                    .children(
                        self.quote_view(
                            &snap,
                            colors,
                            (!wide).then(|| window.viewport_size().height * 0.3),
                        )
                        .map(|quote| quote.flex_shrink_0()),
                    )
                    .children(
                        self.preview
                            .then(|| preview_panel(&snap, colors, self.ui_px)),
                    ),
            )
            .child(attachment_row(&snap, colors, wide).flex_shrink_0())
            .children(feedback)
            .children(show_status.then(|| {
                status_bar(
                    ComposeStatus {
                        mode,
                        note: &hint,
                        draft_save,
                    },
                    &snap,
                    self.send_tried,
                    colors,
                    f32::from(window.viewport_size().width),
                    &|text, size| {
                        text_px(window, text, size, mono.as_deref().unwrap_or(&ui_family))
                    },
                    (&ui_family, &|text, size| {
                        text_px(window, text, size, &ui_family)
                    }),
                )
                .flex_shrink_0()
                .when_some(mono, |bar, family| bar.font_family(family))
            }))
            .children(self.which_visible.then(|| which_key(colors)))
            .children(self.palette.as_ref().map(|query| {
                palette_view(
                    query,
                    palette_help,
                    matches!(lock, None | Some(SendLock::MaybeSent)),
                    colors,
                )
                .when(self.recipients.has_matches(), |panel| {
                    panel.child(div().text_sm().child("Complete recipient"))
                })
            }))
            .children(self.preview.then(|| preview_scrim(colors)))
            .when_some(self.frames.clone(), |root, frames| {
                frames.borrow_mut().built(std::time::Instant::now());
                let prepaint = frames.clone();
                root.child(
                    gpui::canvas(
                        move |_, _, _| prepaint.borrow_mut().mark(1, std::time::Instant::now()),
                        move |_, _, _, _| frames.borrow_mut().painted(std::time::Instant::now()),
                    )
                    .absolute()
                    .size_0(),
                )
            })
    }
}

impl ComposeView {
    #[allow(clippy::too_many_arguments)]
    fn fields_view(
        &self,
        snap: &Snapshot,
        colors: Palette,
        wide: bool,
        window: &Window,
        family: &str,
        reply_check: Option<ReplyCheck>,
        overlay: Option<(Field, gpui::Deferred)>,
    ) -> gpui::Div {
        let mut overlay = overlay;
        let mut overlay_for = |field| {
            if overlay.as_ref().is_some_and(|(anchor, _)| *anchor == field) {
                overlay.take().map(|(_, popup)| popup)
            } else {
                None
            }
        };
        let mut column = div().flex().flex_col().bg(rgb(colors.panel)).px_4().py_1();
        let redirects = redirects(snap);
        // A context row's subject gives way, with an ellipsis, so who and
        // when stay on one line at any width.
        // The padding, border, label and gap around the value, and a
        // margin, since the shaped width runs a little under the painted.
        let row_px = f32::from(window.viewport_size().width) - 32.0 - 10.0 - LABEL_PX - 12.0 - 24.0;
        let fit = |who: &str, what: &str, when: &str| {
            let room = row_px
                - text_px(window, who, self.ui_px, family)
                - text_px(window, when, self.ui_px, family);
            fit_px(what, room, &|text| {
                text_px(window, text, self.ui_px, family)
            })
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
            overlay_for(Field::To),
        );
        // Cc and Bcc always keep a row each, so tabbing into them never
        // moves the body.
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
            overlay_for(Field::Cc),
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
            overlay_for(Field::Bcc),
        );
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
            .children(reply_check.map(|check| reply_to_notice(check, colors)))
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
                // The row, less the fields' padding, the bar's own padding
                // and rule, and a margin for the shaped width running
                // under the painted.
                let room = f32::from(window.viewport_size().width) - 32.0 - 8.0 - 2.0 - 6.0;
                let measure = |text: &str| text_px(window, text, self.ui_px, family);
                Some(hunk_banner(snap, self.focused_hunk, colors, room, &measure))
            } else if snap.send_requested_by.is_some() {
                // The row stays, so an edit arriving later does not push the
                // message down under the reader.
                Some(
                    div()
                        .px_1()
                        .py_1()
                        .border_l(px(2.))
                        .border_color(rgb(colors.panel))
                        .text_color(rgb(colors.dim))
                        .child("no agent edits to review"),
                )
            } else {
                None
            })
    }

    fn body_view(
        &mut self,
        snap: &Snapshot,
        colors: Palette,
        window: &Window,
        family: &str,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let body = if !self.raw && self.field != Field::Body {
            let key = StyledBodyKey {
                draft_id: self.draft_id.clone(),
                revision: snap.revision,
                hunk: self.focused_hunk,
                colors,
                family: family.to_owned(),
                text_px: self.ui_px,
            };
            if self
                .styled_cache
                .as_ref()
                .is_none_or(|(previous, _)| *previous != key)
            {
                let compose = cx.weak_entity();
                let view = cx.new(|_| CachedStyledBody {
                    compose,
                    colors,
                    family: family.to_owned(),
                });
                self.styled_cache = Some((key, view));
            }
            let view = self.styled_cache.as_ref().unwrap().1.clone();
            let mut style = div().flex_1().min_h_0().min_w_0();
            gpui::AnyView::from(view)
                .cached(style.style().clone())
                .into_any_element()
        } else {
            self.styled_cache = None;
            self.body_area(snap, colors, window, family, cx)
                .into_any_element()
        };
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

    fn body_area(
        &self,
        snap: &Snapshot,
        colors: Palette,
        window: &Window,
        family: &str,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        // Room for an agent edit's "was" text: the body, less its padding,
        // the "was" label and its gap, and a margin for the shaped width
        // running under the painted.
        let measure = |text: &str| text_px(window, text, self.ui_px, family);
        let was_room = f32::from(window.viewport_size().width) - 40.0 - measure("was") - 8.0 - 8.0;
        let was_lines = |text: &str| break_lines(text, was_room, &measure);
        let paint = SourcePaint {
            colors,
            text_px: self.ui_px,
            font: gpui::font(if self.raw { "monospace" } else { family }.to_owned()),
            view: cx.weak_entity(),
            row: self.editor.row,
            window,
        };
        div()
            .id("compose-body")
            .text_size(px(self.ui_px))
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .px_5()
            .py_2()
            .child(if self.raw {
                raw_body(&self.editor, &paint)
            } else {
                styled_body(
                    &self.editor,
                    &self.folder,
                    &body_edit_ranges(snap),
                    self.focused_hunk,
                    draft_links(snap),
                    &paint,
                    &was_lines,
                )
            })
    }

    /// The quoted part stored on the draft. Absent when nothing is quoted.
    fn quote_view(
        &self,
        snap: &Snapshot,
        colors: Palette,
        narrow_height: Option<gpui::Pixels>,
    ) -> Option<gpui::Div> {
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
            .text_color(rgb(colors.fg))
            .border_t_1()
            .border_color(rgb(colors.rule))
            .child(label);
        if self.quote_open {
            let quote = render_doc(
                &html_to_doc(html),
                &self.folder,
                Paint {
                    links: Links::Target,
                    text_px: self.ui_px,
                },
                colors,
            );
            block = block.child(if let Some(height) = narrow_height {
                let scroll = self.quote_scroll.clone();
                div()
                    .relative()
                    .child(
                        div()
                            .id("compose-quote")
                            .max_h(height)
                            .overflow_y_scroll()
                            .track_scroll(&self.quote_scroll)
                            .child(quote),
                    )
                    .child(
                        gpui::canvas(
                            |_, _, _| (),
                            move |bounds, _, window, _| {
                                if scroll.max_offset().height + scroll.offset().y > px(1.) {
                                    window.paint_quad(gpui::fill(
                                        bounds,
                                        crate::theme::scroll_fade(colors.bg, false),
                                    ));
                                }
                            },
                        )
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .h(px(24.)),
                    )
            } else {
                quote
            });
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
            if self.quote_open {
                self.quote_scroll.set_offset(gpui::point(px(0.), px(0.)));
            }
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
    /// SMTP authentication rejected the configured credentials.
    CredentialsRejected(String),
    /// The local account settings need attention before this draft can send.
    Unavailable(AccountConfigError),
    /// The connection failed after the server had the whole message. The
    /// draft stays locked so it is not sent twice.
    MaybeSent(String),
}

impl SendFailure {
    fn from_core(err: fell_core::Error) -> Self {
        match err {
            fell_core::Error::SendUncertain(detail) => Self::MaybeSent(detail),
            fell_core::Error::SmtpCredentialsRejected(detail) => Self::CredentialsRejected(detail),
            other => Self::NotSent(other.to_string()),
        }
    }
}

impl std::fmt::Display for SendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Locked(lock) => f.write_str(lock.reason()),
            Self::NotSent(detail) => write!(f, "Not sent: {detail}"),
            Self::CredentialsRejected(_) => write!(f, "SMTP sign-in failed. {SMTP_AUTH_RECOVERY}"),
            Self::Unavailable(error) => write!(f, "Not sent. {error}. {}", error.recovery),
            Self::MaybeSent(detail) => {
                write!(
                    f,
                    "May have been sent. {detail}. Check Sent for a matching copy. A missing copy leaves delivery unknown."
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

const SMTP_AUTH_RECOVERY: &str =
    "Update the account password and run fell account check before retrying.";

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
        Err(_)
            if hub.with_session(id, |session| session.send_lock())
                == Some(Some(SendLock::Sent)) =>
        {
            None
        }
        Err(SendFailure::MaybeSent(_)) => Some(fell_core::compose::MAYBE_SENT_NOTICE.into()),
        Err(SendFailure::NotSent(detail)) => Some(format!("not sent  ·  {detail}")),
        Err(SendFailure::Unavailable(error)) => Some(format!("not sent  ·  {}", error.recovery)),
        Err(SendFailure::CredentialsRejected(_)) => {
            Some(format!("SMTP sign-in failed  ·  {SMTP_AUTH_RECOVERY}"))
        }
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
        Err(
            SendFailure::NotSent(_)
            | SendFailure::CredentialsRejected(_)
            | SendFailure::Unavailable(_),
        ) => None,
    }
}

/// Check and send the draft on screen in one call. Blocks on SMTP, so the
/// window uses [`prepare_from_editor`] and transmits off its thread.
pub fn send_from_editor(
    session: &mut fell_core::compose::DraftSession,
    backend: &dyn MailBackend,
) -> Result<fell_core::SentMessage, SendFailure> {
    let prepared = prepare_from_editor(session)?;
    // This call holds the session throughout, so the claim cannot be lost.
    let result = prepared.transmit_checked(
        backend,
        || true,
        |stored| {
            if stored.is_err() {
                session.report_sent_copy_failure();
            }
        },
    );
    log_send_result(&result);
    session.end_send(lock_after(&result));
    result
}

/// The message the editor is about to send, checked and built while the
/// draft lock is held, so what was on screen is what goes out.
pub struct PreparedSend {
    account: fell_core::AccountId,
    outgoing: fell_core::compose::Outgoing,
    files: Vec<fell_core::SubmittedFile>,
    sent_copy: Option<fell_core::compose::SentCopy>,
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
    let sent_copy = session.sent_copy(&outgoing);
    if let Err(err) = session.begin_send(&outgoing.message_id) {
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
        sent_copy,
    })
}

/// The backend for a send from the window: the demo mailbox, or the account
/// file's login and SMTP settings for this account id. Resolves the account
/// password, so call it off the UI thread.
type SendBackend = Box<dyn MailBackend + Send>;
type StoredSentCopy = fell_core::Result<(
    fell_core::ThreadId,
    fell_core::Message,
    crate::present::Rich,
)>;

fn account_backend(account: &AccountId) -> Result<SendBackend, SendFailure> {
    let config = crate::mailbox::send_account_config(account).map_err(SendFailure::Unavailable)?;
    fell_core::backend_for(account, config.as_ref()).map_err(SendFailure::from_core)
}

/// Transmit a send [`prepare_from_editor`] claimed and record how it ended on
/// the draft, even if the window closed meanwhile. Blocks on SMTP.
pub(crate) fn transmit_from_hub(
    hub: &DraftHub,
    id: &str,
    prepared: PreparedSend,
    backend: impl FnOnce(&AccountId) -> Result<SendBackend, SendFailure>,
) -> (
    Result<fell_core::SentMessage, SendFailure>,
    Option<StoredSentCopy>,
) {
    // While the lock is held every edit path is refused, so the prepared
    // message is still the draft. Only a send that lost its lock stops.
    let mut copy = None;
    let result = backend(&prepared.account).and_then(|backend| {
        prepared.transmit_checked(
            backend.as_ref(),
            || {
                hub.with_session(id, |session| session.send_lock())
                    .flatten()
                    == Some(SendLock::InFlight)
            },
            |stored| copy = Some(stored),
        )
    });
    log_send_result(&result);
    hub.with_session(id, |session| session.end_send(lock_after(&result)));
    (result, copy)
}

fn log_send_result(result: &Result<fell_core::SentMessage, SendFailure>) {
    match result {
        Ok(_) => fell_core::diagnostic!(INFO, "Message sent"),
        Err(SendFailure::MaybeSent(_)) => {
            fell_core::diagnostic!(WARN, "Message delivery uncertain")
        }
        Err(SendFailure::CredentialsRejected(_)) => {
            fell_core::diagnostic!(WARN, "SMTP credentials rejected")
        }
        Err(_) => fell_core::diagnostic!(WARN, "Message not sent"),
    }
}

/// Keep confirmed recipients for completion and Bcc for the Sent copy.
/// A draft whose transmission failed leaves no record.
/// The write runs on its own thread with a few retries: the mail already
/// went out, so a busy mailbox must neither hold up the send nor lose this.
fn remember_sent_recipients(record: fell_core::compose::SentCopy, confirmed: bool) {
    std::thread::spawn(move || {
        if record.remember_recipients(confirmed).is_err() {
            fell_core::diagnostic!(WARN, "Could not record recipients for a send");
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
        saved: impl FnOnce(StoredSentCopy),
    ) -> Result<fell_core::SentMessage, SendFailure> {
        fell_core::diagnostic!(DEBUG, "Preparing SMTP transmission");
        let pending = backend
            .submit_composed(self.outgoing.draft, self.outgoing.mime, self.files)
            .map_err(SendFailure::from_core)?;
        if !still_claimed() {
            return Err(SendFailure::NotSent(
                "the draft is no longer being sent; nothing was sent".into(),
            ));
        }
        let confirm = confirmation_from_ui(&pending);
        let sent_copy = self.sent_copy.map(|record| (record, pending.mime.clone()));
        let result = backend
            .confirm_send(pending, confirm)
            .map_err(SendFailure::from_core);
        if let Some((record, mime)) = sent_copy {
            match &result {
                Ok(_) => {
                    let stored =
                        record
                            .save(&mime, backend.append_sent())
                            .map(|(thread, message)| {
                                let rich = crate::mail_body::prepare_sent(&message);
                                (thread, message, rich)
                            });
                    if stored.is_err() {
                        fell_core::diagnostic!(WARN, "Could not save local Sent copy");
                        remember_sent_recipients(record, true);
                    }
                    saved(stored);
                }
                Err(SendFailure::MaybeSent(_)) => remember_sent_recipients(record, false),
                Err(_) => {}
            }
        }
        result
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
    overlay: Option<gpui::Deferred>,
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
    let inset = crate::recipient_menu::TEXT_INSET;
    parent.child(
        div()
            .flex()
            .gap(px(12. - inset))
            .pl_1()
            .py_1()
            .border_l(px(2.))
            .border_color(rgb(if active { colors.accent } else { colors.panel }))
            .child(
                div()
                    .w(px(LABEL_PX))
                    .flex_shrink_0()
                    .text_color(rgb(colors.fg))
                    .child(label.to_string()),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .child(shown.pl(px(inset)).pr_1())
                    .children(overlay),
            ),
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
const SEND_UNAVAILABLE: &str = "send unavailable for this account";

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
        .flatten()
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
            // The person's own message is "me", as the inbox says it.
            let mine = fell_core::compose::email_of(&snap.from)
                .is_some_and(|me| me.eq_ignore_ascii_case(parent.sender_email.trim()));
            let sender = if mine {
                "me".to_string()
            } else if redirected && parent.sender_email != parent.sender {
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
            if what.chars().count() < 12 && who != parent.sender && !mine {
                // Too little room beside the sender's address: the subject
                // and date take the next line.
                let what = fit("", &subject, &when);
                return context_lines("In reply to", &who, "", None, Some((what, when)), colors);
            }
            context_row("In reply to", &who, &what, Some(when), colors)
        }
        // The original is gone from the mailbox: said in words, as the
        // drafts list says it, not as a raw Message-ID.
        (None, Some(id)) if !id.trim().is_empty() => {
            context_row("In reply to", crate::present::REPLY_GONE, "", None, colors)
        }
        _ => context_row("In reply to", "none (new message)", "", None, colors),
    }
}

fn reply_to_notice(check: ReplyCheck, colors: Palette) -> gpui::Div {
    let (title, detail, color) = match check {
        ReplyCheck::Waiting => (
            "Reply-To unverified",
            "Enter recipients, or sync from the inbox to check.",
            colors.error,
        ),
        ReplyCheck::Checking => (
            "Checking Reply-To…",
            "Enter recipients now, or wait for verification.",
            colors.accent,
        ),
        ReplyCheck::Unavailable => (
            "Reply-To check unavailable",
            "Enter recipients, or retry sync from the inbox.",
            colors.error,
        ),
        ReplyCheck::Verified => (
            "Reply-To verified",
            "Start a new reply from the inbox to fill in recipients.",
            colors.accent,
        ),
    };
    div()
        .my_1()
        .px_2()
        .py_1()
        .border_l(px(2.))
        .border_color(rgb(color))
        .text_size(px(15.))
        .child(
            div()
                .font_weight(FontWeight(620.))
                .text_color(rgb(color))
                .child(title),
        )
        .child(detail)
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

/// The agent-edit bar: one line at every width. What the edit is and where
/// it sits, then the keys, joined by dots. The keys never wrap or cut:
/// the words give way first (fewer of them, then none, then the name),
/// and only a window too narrow even for that drops the move keys.
fn hunk_banner(
    snap: &Snapshot,
    focused: Option<u64>,
    colors: Palette,
    room: f32,
    measure: &dyn Fn(&str) -> f32,
) -> gpui::Div {
    let parts = hunk_banner_parts(snap, focused, room, measure);
    let mut row = div()
        .flex()
        .items_center()
        .gap_2()
        .overflow_hidden()
        .whitespace_nowrap()
        .px_1()
        .py_1()
        .border_l(px(2.))
        .border_color(rgb(colors.rule))
        .bg(rgb(colors.hunk))
        .text_color(rgb(colors.fg));
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            row = row.child(div().flex_shrink_0().text_color(rgb(colors.dim)).child("·"));
        }
        row = row.child(div().flex_shrink_0().child(part));
    }
    row
}

/// The bar's parts, the longest set that fits `room` as `measure` lays
/// them out: each part's width plus a dot and 8px either side between.
fn hunk_banner_parts(
    snap: &Snapshot,
    focused: Option<u64>,
    room: f32,
    measure: &dyn Fn(&str) -> f32,
) -> Vec<String> {
    let order = ordered_hunks(snap);
    let at = focused
        .and_then(|focused| order.iter().position(|(hunk, _)| *hunk == focused))
        .unwrap_or(0);
    let hunk = order_hunk(snap, &order, at);
    let count = format!("{} of {}", at + 1, order.len().max(1));
    // Moving between edits needs two of them.
    let keys: &[&str] = if order.len() > 1 {
        &["]h [h move", "space a k keep", "space a x reject"]
    } else {
        &["space a k keep", "space a x reject"]
    };
    // The edit's names, longest first: its first plain words, then what it
    // changed in two words.
    let mut names: Vec<String> = Vec::new();
    match hunk.map(|hunk| hunk.kind) {
        Some(HunkKind::Recipients) => names.push("agent changed recipients".into()),
        Some(HunkKind::Subject) => names.push("agent changed the subject".into()),
        Some(HunkKind::Attachments) => names.push("agent changed attachments".into()),
        Some(HunkKind::Body) => {
            let hunk = hunk.expect("a body hunk");
            let mut last = None;
            for words in (1..=BANNER_WORDS).rev() {
                let Some(words) = edit_preview_words(snap, hunk, words) else {
                    continue;
                };
                // Fewer words can give the same text (a short edit).
                if last.as_ref() != Some(&words) {
                    names.push(format!("agent edit \u{201c}{words}\u{201d}"));
                }
                last = Some(words);
            }
        }
        None => {}
    }
    names.push("agent edit".into());
    let dot = measure("·") + 16.;
    let width = |parts: &[String]| -> f32 {
        parts.iter().map(|part| measure(part)).sum::<f32>()
            + parts.len().saturating_sub(1) as f32 * dot
    };
    let with_keys = |lead: &[&str], keys: &[&str]| -> Vec<String> {
        lead.iter()
            .chain(keys)
            .map(|part| part.to_string())
            .collect()
    };
    let review = &keys[keys.len() - 2..];
    let mut candidates: Vec<Vec<String>> = names
        .iter()
        .map(|name| with_keys(&[name, &count], keys))
        .collect();
    // "1 of 1" says less than what the edit is: it goes first.
    if order.len() <= 1 {
        candidates.extend(names.iter().map(|name| with_keys(&[name], keys)));
    }
    if order.len() > 1 {
        candidates.push(with_keys(&[&count], keys));
        candidates.push(with_keys(&[&count], review));
    }
    candidates.push(with_keys(&[], review));
    let fallback = candidates.last().cloned().unwrap_or_default();
    candidates
        .into_iter()
        .find(|parts| width(parts) <= room)
        .unwrap_or(fallback)
}

/// At most this many of an edit's words in the bar: enough to tell edits
/// apart; the tinted block below gives the whole text.
const BANNER_WORDS: usize = 5;

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

/// The width of one line of text at `size` pixels in `family`, the face
/// the row paints in (the window's own default face is a different one).
fn text_px(window: &Window, text: &str, size: f32, family: &str) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let mut style = window.text_style();
    style.font_family = SharedString::from(family.to_string());
    let run = style.to_run(text.len());
    let line = window.text_system().shape_line(
        SharedString::from(text.to_string()),
        px(size),
        &[run],
        None,
    );
    f32::from(line.width)
}

/// The edit's first words, at most `limit`, with an ellipsis when cut: as
/// plain text, with markdown marks, link destinations and bare URLs gone.
fn edit_preview_words(
    snap: &Snapshot,
    hunk: &fell_core::compose::HunkView,
    limit: usize,
) -> Option<String> {
    let slice = snap.text.get(hunk.start..hunk.end)?;
    let line = slice
        .lines()
        .map(plain_words)
        .find(|line| !line.is_empty())?;
    let words: Vec<&str> = line.split_whitespace().take(limit).collect();
    if words.is_empty() {
        return None;
    }
    let more = line.split_whitespace().count() > words.len();
    let mut text = words.join(" ");
    if more {
        // "attached…", not "attached,…".
        text = text
            .trim_end_matches([',', ';', ':', '.', '-', '\u{2014}', '\u{2013}'])
            .to_string();
        text.push('…');
    }
    Some(text)
}

/// A markdown line as a reader says it: a link as its label, no bare URLs,
/// no list, heading or emphasis marks.
fn plain_words(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find("](") else {
            break;
        };
        let target = &after[close + 2..];
        let Some(end) = target.find(')') else {
            break;
        };
        // An image's `!` goes with its link.
        out.push_str(rest[..open].strip_suffix('!').unwrap_or(&rest[..open]));
        out.push_str(&after[..close]);
        rest = &target[end + 1..];
    }
    out.push_str(rest);
    let words: Vec<&str> = out
        .split_whitespace()
        .filter(|word| !looks_like_url(word))
        .collect();
    plain_markdown(&words.join(" "))
}

/// A word that is a web or mail address written out: `https://…`,
/// `www.…`, `mailto:…`, or one in angle brackets.
fn looks_like_url(word: &str) -> bool {
    let word = word.trim_start_matches(['<', '(']).to_ascii_lowercase();
    ["http://", "https://", "www.", "mailto:"]
        .iter()
        .any(|scheme| word.starts_with(scheme))
}

fn order_hunk<'a>(
    snap: &'a Snapshot,
    order: &[(u64, Option<usize>)],
    at: usize,
) -> Option<&'a fell_core::compose::HunkView> {
    let (id, _) = order.get(at)?;
    snap.hunks.iter().find(|hunk| hunk.id == *id)
}

/// How the status line names an edit: what it changed. Never its words:
/// the agent-edit bar and the tinted block already give them, and on a
/// narrow window they would push out the key that matters (`u undo`).
fn edit_name(hunk: &fell_core::compose::HunkView) -> String {
    match hunk.kind {
        HunkKind::Recipients => "the recipient change".into(),
        HunkKind::Subject => "the subject change".into(),
        HunkKind::Attachments => "the attachment change".into(),
        HunkKind::Body => "the edit".into(),
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
    paint: &SourcePaint,
    was_lines: &dyn Fn(&str) -> Vec<String>,
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
            BodyItem::CaretRow => (None, cursor_line_row(editor, paint)),
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
                    raw_source_block(
                        block, &text, editor, &edits, focused, links, paint, was_lines,
                    )
                } else {
                    render_block(
                        block,
                        folder,
                        Paint {
                            links,
                            text_px: paint.text_px,
                        },
                        paint.colors,
                    )
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

fn cursor_line_row(editor: &Editor, paint: &SourcePaint) -> gpui::Div {
    let line = editor
        .lines
        .get(editor.row)
        .map(String::as_str)
        .unwrap_or("");
    raw_line(
        line,
        Some(editor.col),
        Urls::Source,
        editor.mode,
        false,
        paint,
    )
}

fn raw_body(editor: &Editor, paint: &SourcePaint) -> gpui::Div {
    let mut column = div().flex().flex_col().font_family("monospace");
    for (index, line) in editor.lines.iter().enumerate() {
        let cursor = !editor.caret_hidden && index == editor.row;
        column = column.child(raw_line(
            line,
            cursor.then_some(editor.col),
            Urls::Source,
            editor.mode,
            false,
            paint,
        ));
    }
    column
}

#[allow(clippy::too_many_arguments)]
fn raw_source_block(
    block: &fell_core::compose::Block,
    text: &str,
    editor: &Editor,
    edits: &[&(usize, usize, String, u64)],
    focused: Option<u64>,
    links: Links,
    paint: &SourcePaint,
    was_lines: &dyn Fn(&str) -> Vec<String>,
) -> gpui::Div {
    let colors = paint.colors;
    let Some((start, end)) = block.source else {
        return render_block(
            block,
            Path::new(""),
            Paint {
                links,
                text_px: paint.text_px,
            },
            colors,
        );
    };
    let slice = &text[start.min(text.len())..end.min(text.len())];
    let mut column = div().flex().flex_col();
    let base_line = text[..start].chars().filter(|ch| *ch == '\n').count();
    let mut line_start = start;
    let mut announced = Vec::new();
    // The agent edit being painted: its lines and what they replaced share
    // one tinted block, so no seam of page colour shows between them.
    let mut tint: Option<(gpui::Div, bool)> = None;
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
        let tinted = !touching.is_empty();
        let urls = match (cursor, tinted) {
            (Some(_), _) => Urls::Source,
            (None, true) => Urls::Bare,
            (None, false) => Urls::Fold(links),
        };
        let painted = raw_line(line, cursor, urls, editor.mode, tinted, paint);
        if !tinted {
            if let Some((block, _)) = tint.take() {
                column = column.child(block);
            }
            column = column.child(painted);
            line_start = line_end + 1;
            continue;
        }
        // The focused edit carries an accent bar, so the body shows which
        // one the bar, keep and reject are about.
        let is_focused = touching
            .iter()
            .any(|index| Some(edits[*index].3) == focused);
        // A new edit right under another one starts its own block.
        let fresh = touching.iter().any(|index| !announced.contains(index));
        let (mut block, gap) = match tint.take() {
            Some((block, was_focused)) if !fresh && was_focused == is_focused => (block, false),
            Some((block, _)) => {
                column = column.child(block);
                (hunk_block(is_focused, colors), fresh)
            }
            None => (hunk_block(is_focused, colors), false),
        };
        if gap {
            block = block.mt(px(6.));
        }
        for index in &touching {
            if announced.contains(index) {
                continue;
            }
            announced.push(*index);
            let previous = plain_markdown(&edits[*index].2);
            if !previous.is_empty() {
                block = block.child(
                    div()
                        .flex()
                        .gap_2()
                        .text_color(rgb(colors.dim))
                        .child(div().flex_shrink_0().child("was"))
                        // Broken into lines here, not by the text layout,
                        // so a line's strike ends at its last word, never
                        // through the space where it wrapped.
                        .child(div().flex().flex_col().min_w(px(0.)).children(
                            was_lines(&previous).into_iter().map(|line| {
                                div()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .line_through()
                                    .child(line)
                            }),
                        )),
                );
            }
        }
        tint = Some((block.child(painted), is_focused));
        line_start = line_end + 1;
    }
    if let Some((block, _)) = tint {
        column = column.child(block);
    }
    column
}

/// The tinted block an agent edit's lines sit in. Its 3px bar sits in the
/// margin, so the text lines up with the lines around the block.
fn hunk_block(focused: bool, colors: Palette) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .overflow_hidden()
        .ml(px(-3.))
        .bg(rgb(if focused {
            colors.hunk_focus
        } else {
            colors.hunk
        }))
        .border_l(px(3.))
        .border_color(rgb(if focused { colors.accent } else { colors.hunk }))
}

/// How a source line shows a link's `](url)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Urls {
    /// As written: the cursor line and the raw view.
    Source,
    /// The brackets fold like the other markers, and the whole target
    /// shows after the label: a line an agent changed, off the cursor, so
    /// a new destination is seen.
    Bare,
    /// Folded away, with the destination after the label when `Links`
    /// says the reader needs it, as the styled body shows it.
    Fold(Links),
}

/// Whether a span of a source line is left out: markers off the cursor
/// line, and a link's `](url)` when URLs fold.
fn folds(kind: StyleKind, show_markers: bool, urls: Urls) -> bool {
    match kind {
        StyleKind::Marker => !show_markers,
        StyleKind::LinkUrl => urls != Urls::Source,
        _ => false,
    }
}

/// What a source line shows after a link's `](url)` span, if anything:
/// where a folded link goes when the label alone could mislead, or an
/// IDN host in `xn--` form beside a URL shown as written.
fn url_note(label: &str, url_span: &str, urls: Urls) -> Option<String> {
    let href = span_href(url_span)?;
    match urls {
        Urls::Source => idn_place(href).map(|place| format!("({place})")),
        Urls::Bare => Some(format!("({})", ascii_target(href))),
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

struct SourcePaint<'a> {
    colors: Palette,
    font: gpui::Font,
    text_px: f32,
    view: gpui::WeakEntity<ComposeView>,
    row: usize,
    window: &'a Window,
}

/// One source line. On an agent edit's `tinted` background, a link's URL
/// and where it goes take the body colour, so they keep AA there.
fn raw_line(
    line: &str,
    cursor: Option<usize>,
    urls: Urls,
    mode: Mode,
    tinted: bool,
    paint: &SourcePaint,
) -> gpui::Div {
    if let Some(col) = cursor
        && line.contains("](")
    {
        let styles = line_styles(line);
        if has_source_link(line, &styles) {
            return focused_link_line(line, &styles, col, mode, tinted, paint);
        }
    }
    let row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .w_full()
        .min_h(px(24.))
        .items_center();
    let colors = paint.colors;
    source_pieces(
        line,
        cursor,
        urls == Urls::Source,
        urls,
        mode,
        tinted,
        colors,
    )
    .into_row(row, colors)
}

fn has_source_link(line: &str, styles: &[StyleSpan]) -> bool {
    styles.iter().enumerate().any(|(index, span)| {
        span.kind == StyleKind::LinkUrl
            && styles[..index]
                .iter()
                .rev()
                .find(|span| span.kind != StyleKind::LinkText)
                .is_some_and(|span| {
                    span.kind == StyleKind::Marker && &line[span.start..span.end] == "["
                })
    })
}

struct FocusedLinkText {
    text: String,
    runs: Vec<gpui::TextRun>,
    /// Display byte ranges with no counterpart in the Markdown.
    notes: Vec<Range<usize>>,
    insert_caret: Option<usize>,
}

fn focused_link_text(
    line: &str,
    styles: &[StyleSpan],
    col: usize,
    mode: Mode,
    tinted: bool,
    font: &gpui::Font,
    colors: Palette,
) -> FocusedLinkText {
    let quiet = if tinted { colors.fg } else { colors.muted };
    let cursor = source_cursor_byte(line, Some(col), mode).unwrap_or(line.len());
    let mut text = String::with_capacity(line.len());
    let mut runs = Vec::new();
    let mut notes = Vec::new();
    let mut insert_caret = (mode == Mode::Insert).then_some(cursor);
    for span in styles {
        let slice = &line[span.start..span.end];
        text.push_str(slice);
        // The closing label bracket takes the same color as the opening one.
        let close =
            span.start + usize::from(span.kind == StyleKind::LinkUrl && slice.starts_with(']'));
        for (range, kind) in [
            (span.start..close, StyleKind::Marker),
            (close..span.end, span.kind),
        ] {
            if range.is_empty() {
                continue;
            }
            let mut font = font.clone();
            match kind {
                StyleKind::Strong => font.weight = FontWeight(700.),
                StyleKind::Em => font.style = gpui::FontStyle::Italic,
                StyleKind::Code => font.family = "monospace".into(),
                _ => {}
            }
            let run = gpui::TextRun {
                len: range.len(),
                font,
                color: rgb(source_color(kind, quiet, colors)).into(),
                background_color: (kind == StyleKind::Code).then(|| rgb(colors.code).into()),
                underline: (kind == StyleKind::LinkText).then_some(gpui::UnderlineStyle {
                    thickness: px(1.),
                    ..Default::default()
                }),
                strikethrough: None,
            };
            if mode != Mode::Insert && range.contains(&cursor) {
                let glyph_len = line[cursor..].chars().next().map_or(0, char::len_utf8);
                runs.extend([
                    gpui::TextRun {
                        len: cursor - range.start,
                        ..run.clone()
                    },
                    gpui::TextRun {
                        len: glyph_len,
                        color: rgb(colors.bg).into(),
                        background_color: Some(rgb(colors.caret).into()),
                        ..run.clone()
                    },
                    gpui::TextRun {
                        len: range.end - cursor - glyph_len,
                        ..run
                    },
                ]);
            } else {
                runs.push(run);
            }
        }
        if span.kind == StyleKind::LinkUrl
            && let Some(note) = url_note("", slice, Urls::Source)
        {
            let start = text.len();
            text.push(' ');
            text.push_str(&display_safe(&note));
            let len = text.len() - start;
            notes.push(start..text.len());
            runs.push(gpui::TextRun {
                len,
                font: font.clone(),
                color: rgb(quiet).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            });
            if cursor >= span.end
                && let Some(at) = &mut insert_caret
            {
                *at += len;
            }
        }
    }
    FocusedLinkText {
        text,
        runs,
        notes,
        insert_caret,
    }
}

fn source_col(line: &str, notes: &[Range<usize>], display_byte: usize) -> usize {
    let added: usize = notes
        .iter()
        .take_while(|note| note.start < display_byte)
        .map(|note| note.end.min(display_byte) - note.start)
        .sum();
    line[..(display_byte - added).min(line.len())]
        .chars()
        .count()
}

/// Pick soft breaks from the shaped advances. Prefer a nearby separator,
/// but use the available space when a URL segment stretches past the edge.
fn focused_link_breaks(
    text: &str,
    shaped: &gpui::WrappedLine,
    width: gpui::Pixels,
) -> Option<Vec<usize>> {
    let mut stops: Vec<_> = shaped
        .runs()
        .iter()
        .flat_map(|run| {
            run.glyphs
                .iter()
                .map(|glyph| (glyph.index, glyph.position.x))
        })
        .collect();
    if !stops.iter().map(|(byte, _)| byte).is_sorted() || !stops.iter().map(|(_, x)| x).is_sorted()
    {
        // Let the platform wrap text whose glyphs reordered for bidi.
        return None;
    }
    stops.dedup_by_key(|(byte, _)| *byte);
    stops.push((text.len(), shaped.unwrapped_layout.width));
    let mut breaks = Vec::new();
    let mut start = 0;
    while start + 1 < stops.len() {
        let limit = stops[start].1 + width;
        let fit = (stops
            .partition_point(|(_, x)| *x <= limit)
            .saturating_sub(1))
        .max(start + 1);
        if fit + 1 == stops.len() {
            break;
        }
        let near_edge = limit - width / 4.;
        let next = (start + 1..=fit)
            .rev()
            .take_while(|&index| stops[index].1 >= near_edge)
            .find(|&index| {
                let at = stops[index].0;
                text[..at].ends_with([' ', '-', '.', '?', '&', '=', '#'])
                    || text[..at].ends_with([':', '/']) && !text[at..].starts_with('/')
            })
            .unwrap_or(fit);
        breaks.push(stops[next].0);
        start = next;
    }
    Some(breaks)
}

/// Materialize visual rows for GPUI while keeping their break bytes out of
/// the editor's source coordinates. Each inserted newline belongs to the
/// preceding text run.
fn wrap_focused_link(
    text: &str,
    runs: Vec<gpui::TextRun>,
    breaks: &[usize],
) -> (String, Vec<gpui::TextRun>, Vec<usize>) {
    let mut wrapped = String::with_capacity(text.len() + breaks.len());
    let mut wrapped_runs = Vec::new();
    let mut newlines = Vec::with_capacity(breaks.len());
    let mut breaks = breaks.iter().copied().peekable();
    let mut start = 0;
    for run in runs {
        let end = start + run.len;
        while let Some(&at) = breaks.peek().filter(|&&at| at <= end) {
            wrapped.push_str(&text[start..at]);
            newlines.push(wrapped.len());
            wrapped.push('\n');
            wrapped_runs.push(gpui::TextRun {
                len: at - start + 1,
                ..run.clone()
            });
            breaks.next();
            start = at;
        }
        if end > start {
            wrapped.push_str(&text[start..end]);
            wrapped_runs.push(gpui::TextRun {
                len: end - start,
                ..run
            });
        }
        start = end;
    }
    (wrapped, wrapped_runs, newlines)
}

/// Shape the whole source line so style and caret boundaries cannot split
/// it into flex columns. The same layout supplies the click and caret positions.
fn focused_link_line(
    line: &str,
    styles: &[StyleSpan],
    col: usize,
    mode: Mode,
    tinted: bool,
    paint: &SourcePaint,
) -> gpui::Div {
    let FocusedLinkText {
        text,
        runs,
        notes,
        mut insert_caret,
    } = focused_link_text(line, styles, col, mode, tinted, &paint.font, paint.colors);
    // The compose body fills the window, less its two px_5 gutters.
    let width = (paint.window.viewport_size().width - paint.window.rem_size() * 2.5).max(px(1.));
    let breaks = paint
        .window
        .text_system()
        .shape_text(text.clone().into(), px(paint.text_px), &runs, None, None)
        .ok()
        .and_then(|lines| focused_link_breaks(&text, lines.first()?, width));
    let flat_wrap = breaks.is_some();
    let (text, runs, newlines) = match breaks {
        Some(breaks) if !breaks.is_empty() => {
            if let Some(at) = &mut insert_caret {
                *at += breaks.partition_point(|break_at| *break_at <= *at);
            }
            wrap_focused_link(&text, runs, &breaks)
        }
        _ => (text, runs, Vec::new()),
    };
    let text = gpui::StyledText::new(text).with_runs(runs);
    let layout = text.layout().clone();
    let click_layout = layout.clone();
    let view = paint.view.clone();
    let row = paint.row;
    let line = line.to_owned();
    let caret_color = paint.colors.caret;
    div()
        .relative()
        .flex()
        .flex_col()
        .w_full()
        .min_w(px(0.))
        .min_h(px(24.))
        .cursor_text()
        .when(flat_wrap, |row| row.whitespace_nowrap())
        .on_mouse_down(gpui::MouseButton::Left, move |event, _, cx| {
            let mut position = event.position - click_layout.bounds().origin;
            let height = click_layout.line_height();
            let row_start = if flat_wrap {
                let visual_row = (position.y / height) as usize;
                position.y = px(0.);
                if visual_row == 0 {
                    0
                } else {
                    let Some(start) = newlines.get(visual_row - 1) else {
                        return;
                    };
                    start + 1
                }
            } else {
                0
            };
            let Some(shaped) = click_layout.line_layout_for_index(row_start) else {
                return;
            };
            let index = if mode == Mode::Insert {
                shaped.closest_index_for_position(position, height)
            } else {
                shaped.index_for_position(position, height)
            };
            let byte = row_start + index.unwrap_or_else(|end| end);
            let byte = byte - newlines.partition_point(|newline| *newline < byte);
            let col = source_col(&line, &notes, byte);
            let _ = view.update(cx, |this, cx| this.place_link_cursor(row, &line, col, cx));
        })
        .child(text)
        .children(insert_caret.map(|at| {
            gpui::canvas(
                |_, _, _| (),
                move |_, (), window, _| {
                    let Some(mut position) = layout.position_for_index(at) else {
                        return;
                    };
                    let line_height = layout.line_height();
                    // GPUI assigns the wrap boundary to the preceding row.
                    // Draw before the next character on the continuation instead.
                    if !flat_wrap
                        && let Some(shaped) = layout.line_layout_for_index(at)
                        && shaped.wrap_boundaries().iter().any(|wrap| {
                            shaped.runs()[wrap.run_ix].glyphs[wrap.glyph_ix].index == at
                        })
                    {
                        position.x = layout.bounds().left();
                        position.y += line_height;
                    }
                    window.paint_quad(gpui::fill(
                        gpui::Bounds::new(position, gpui::size(px(2.), line_height)),
                        rgb(caret_color),
                    ));
                },
            )
            .absolute()
            .inset_0()
        }))
}

fn source_cursor_byte(line: &str, cursor: Option<usize>, mode: Mode) -> Option<usize> {
    cursor.map(|col| {
        let col = if mode == Mode::Insert {
            col
        } else {
            col.min(line.chars().count().saturating_sub(1))
        };
        byte_index(line, col)
    })
}

fn source_color(kind: StyleKind, quiet: u32, colors: Palette) -> u32 {
    match kind {
        StyleKind::Marker => colors.marker,
        StyleKind::LinkUrl => quiet,
        StyleKind::LinkText => colors.accent,
        _ => colors.fg,
    }
}

/// What a source line shows, piece by piece.
fn source_pieces(
    line: &str,
    cursor: Option<usize>,
    show_markers: bool,
    urls: Urls,
    mode: Mode,
    tinted: bool,
    colors: Palette,
) -> Pieces {
    let quiet = if tinted { colors.fg } else { colors.muted };
    let styles = line_styles(line);
    if styles.is_empty() {
        let mut pieces = Pieces::default();
        if cursor.is_some() {
            pieces.push(Piece::Caret(mode));
        }
        return pieces;
    }
    let cursor_byte = source_cursor_byte(line, cursor, mode);
    let mut pieces = Pieces::default();
    let mut label = "";
    for span in styles {
        let span_start = span.start.min(line.len());
        let span_end = span.end.min(line.len());
        let slice = &line[span_start..span_end];
        if span.kind == StyleKind::LinkText {
            label = slice;
        }
        let note = (span.kind == StyleKind::LinkUrl)
            .then(|| url_note(label, slice, urls))
            .flatten();
        if folds(span.kind, show_markers, urls) {
            if span.start == 0 && span.kind == StyleKind::Marker {
                pieces.drawn(folded_marker(line, colors));
            }
            if let Some(note) = note {
                pieces.note(note, quiet);
            }
            continue;
        }
        if span.start == 0
            && span.kind == StyleKind::Marker
            && matches!(slice, "- " | "-" | "* " | "*")
        {
            pieces.drawn(
                div()
                    .font_weight(FontWeight(400.))
                    .text_color(rgb(colors.marker))
                    .pr(px(6.))
                    .child(slice.trim().to_string()),
            );
            continue;
        }
        let weight = match span.kind {
            StyleKind::Strong => FontWeight(700.),
            _ => FontWeight(400.),
        };
        let source = Source {
            line,
            cursor: cursor_byte,
            mode,
        };
        if span.kind == StyleKind::LinkUrl && slice.starts_with(']') {
            // `]` is a marker like the `[` before the label, so both
            // brackets take the same colour.
            source.paint(
                &mut pieces,
                (span_start, span_start + 1),
                colors.marker,
                weight,
                StyleKind::Marker,
            );
            source.paint(
                &mut pieces,
                (span_start + 1, span_end),
                quiet,
                weight,
                span.kind,
            );
        } else {
            let color = source_color(span.kind, quiet, colors);
            source.paint(
                &mut pieces,
                (span_start, span_end),
                color,
                weight,
                span.kind,
            );
        }
        if let Some(note) = note {
            pieces.note(note, quiet);
        }
    }
    if cursor_byte.is_some_and(|at| at >= line.len()) {
        pieces.push(Piece::Caret(mode));
    }
    pieces
}

/// The pieces of a wrapping line of text. The spaces between two pieces
/// join the end of the first one, so a piece the row wraps onto a new
/// line starts at the margin, not a space in.
#[derive(Default)]
struct Pieces(Vec<Piece>);

enum Piece {
    /// Text in one style.
    Span {
        text: String,
        color: u32,
        weight: FontWeight,
        kind: StyleKind,
    },
    /// The glyph under the normal-mode cursor.
    Glyph {
        text: String,
        weight: FontWeight,
        kind: StyleKind,
    },
    Caret(Mode),
    /// Text in the style around it.
    Words(String),
    /// A link's label in the weight around it.
    Link(String),
    /// Anything else, drawn already.
    Drawn(Box<gpui::Div>),
    /// A piece and the spaces after it, which stay on its line.
    Spaced(Box<Piece>, String),
}

impl Piece {
    /// The piece as row items, word by word, each with whether the row may
    /// break after it: only after a space, as in running text.
    fn render_into(self, out: &mut Vec<(gpui::Div, bool)>, colors: Palette) {
        match self {
            Piece::Span {
                text,
                color,
                weight,
                kind,
            } => {
                for word in words(&text) {
                    let item = styled_span(word, color, weight, kind, colors.code);
                    out.push((item.min_w(px(0.)), word.ends_with(' ')));
                }
            }
            Piece::Words(text) => {
                for word in words(&text) {
                    let item = div().min_w(px(0.)).child(word.to_string());
                    out.push((item, word.ends_with(' ')));
                }
            }
            Piece::Link(text) => {
                for word in words(&text) {
                    let item = div()
                        .min_w(px(0.))
                        .text_color(rgb(colors.accent))
                        .underline()
                        .child(word.to_string());
                    out.push((item, word.ends_with(' ')));
                }
            }
            Piece::Glyph { text, weight, kind } => {
                let breaks = text == " ";
                out.push((inverted_span(&text, weight, kind, colors), breaks));
            }
            Piece::Caret(mode) => out.push((caret(colors, mode), false)),
            Piece::Drawn(div) => out.push((*div, false)),
            Piece::Spaced(piece, spaces) => {
                piece.render_into(out, colors);
                let gap = div().flex_shrink_0().child(spaces);
                match out.pop() {
                    Some((last, _)) => {
                        out.push((div().flex().min_w(px(0.)).child(last).child(gap), true))
                    }
                    None => out.push((gap, true)),
                }
            }
        }
    }
}

/// A run of text cut after each run of spaces: "is attached, " gives
/// "is ", "attached, ". A leading indent stays with the first word.
fn words(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let lead = rest.len() - rest.trim_start_matches(' ').len();
        let word = rest[lead..].find(' ').map_or(rest.len(), |at| lead + at);
        let end = word + (rest[word..].len() - rest[word..].trim_start_matches(' ').len());
        let (head, tail) = rest.split_at(end);
        rest = tail;
        Some(head)
    })
}

impl Pieces {
    fn push(&mut self, piece: Piece) {
        self.0.push(piece);
    }

    fn drawn(&mut self, piece: gpui::Div) {
        self.push(Piece::Drawn(Box::new(piece)));
    }

    /// Spaces after the last piece, wherever the row breaks.
    fn space(&mut self, spaces: &str) {
        if spaces.is_empty() {
            return;
        }
        match self.0.pop() {
            Some(Piece::Spaced(piece, mut more)) => {
                more.push_str(spaces);
                self.push(Piece::Spaced(piece, more));
            }
            Some(last) => self.push(Piece::Spaced(Box::new(last), spaces.to_string())),
            None => self.push(Piece::Words(spaces.to_string())),
        }
    }

    /// A note after a link, a space away from it.
    fn note(&mut self, note: String, color: u32) {
        self.space(" ");
        self.push(Piece::Span {
            text: display_safe(&note),
            color,
            weight: FontWeight(400.),
            kind: StyleKind::Plain,
        });
    }

    /// Text whose leading spaces join the piece before it. A line's own
    /// indent stays where it is.
    fn text(&mut self, text: &str, paint: impl FnOnce(String) -> Piece) {
        let rest = text.trim_start_matches(' ');
        if self.0.is_empty() || rest.len() == text.len() {
            if !text.is_empty() {
                self.push(paint(text.to_string()));
            }
            return;
        }
        self.space(&text[..text.len() - rest.len()]);
        if !rest.is_empty() {
            self.push(paint(rest.to_string()));
        }
    }

    /// The row, flowing like running text: it breaks only after a space,
    /// so a word split by the cursor or by a style change (`plan](url)`)
    /// stays whole, and a word wider than the row breaks inside itself.
    fn into_row(self, row: gpui::Div, colors: Palette) -> gpui::Div {
        let mut items = Vec::new();
        for piece in self.0 {
            piece.render_into(&mut items, colors);
        }
        let mut row = row;
        let mut word: Vec<gpui::Div> = Vec::new();
        let flush = |row: gpui::Div, word: &mut Vec<gpui::Div>| match word.len() {
            0 => row,
            1 => row.children(word.drain(..)),
            _ => row.child(div().flex().min_w(px(0.)).children(word.drain(..))),
        };
        for (item, breaks) in items {
            word.push(item);
            if breaks {
                row = flush(row, &mut word);
            }
        }
        flush(row, &mut word)
    }
}

/// A source line being painted, with the cursor in it if it has one.
struct Source<'a> {
    line: &'a str,
    cursor: Option<usize>,
    mode: Mode,
}

impl Source<'_> {
    /// Bytes `start..end` of the line in one style, split where the cursor
    /// sits in them.
    fn paint(
        &self,
        pieces: &mut Pieces,
        (start, end): (usize, usize),
        color: u32,
        weight: FontWeight,
        kind: StyleKind,
    ) {
        let slice = &self.line[start..end];
        let span = |text: String| Piece::Span {
            text,
            color,
            weight,
            kind,
        };
        let Some(at) = self.cursor.filter(|at| *at >= start && *at < end) else {
            pieces.text(slice, span);
            return;
        };
        let (left, right) = split_at_char_boundary(slice, at - start);
        pieces.text(left, span);
        if self.mode != Mode::Insert
            && let Some(ch) = right.chars().next()
        {
            let (glyph, rest) = right.split_at(ch.len_utf8());
            pieces.push(Piece::Glyph {
                text: glyph.to_string(),
                weight,
                kind,
            });
            pieces.text(rest, span);
        } else {
            pieces.push(Piece::Caret(self.mode));
            pieces.text(right, span);
        }
    }
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

/// A compose draft in the inbox's reading pane: the compose window's
/// rendering, at the inbox's text size. Links show where they go by the
/// editor's rules: an agent's draft shows each link's host, the person's
/// own a label that reads as an address and goes elsewhere. Its text loses
/// bidi and invisible format characters, as headers do.
pub(crate) fn render_draft_doc(
    doc: &StyledDoc,
    agent: bool,
    theme: &crate::theme::Theme,
) -> gpui::Div {
    let paint = Paint {
        links: if agent { Links::Host } else { Links::Label },
        text_px: theme.ui_px,
    };
    render_doc(
        &safe_doc(doc),
        Path::new(""),
        paint,
        palette_from_theme(theme),
    )
}

/// `doc` with each text run through [`text_safe`].
fn safe_doc(doc: &StyledDoc) -> StyledDoc {
    fn inlines(items: &[fell_core::compose::Inline]) -> Vec<fell_core::compose::Inline> {
        use fell_core::compose::Inline;
        items
            .iter()
            .map(|item| match item {
                Inline::Text(text) => Inline::Text(text_safe(text)),
                Inline::Code(text) => Inline::Code(text_safe(text)),
                Inline::Em(inner) => Inline::Em(inlines(inner)),
                Inline::Strong(inner) => Inline::Strong(inlines(inner)),
                Inline::Link { text, href } => Inline::Link {
                    text: text_safe(text),
                    href: href.clone(),
                },
                Inline::Image { alt, src } => Inline::Image {
                    alt: text_safe(alt),
                    src: src.clone(),
                },
            })
            .collect()
    }
    fn safe_block(block: &fell_core::compose::Block) -> fell_core::compose::Block {
        let body = match &block.body {
            BlockBody::Heading {
                level,
                inlines: items,
            } => BlockBody::Heading {
                level: *level,
                inlines: inlines(items),
            },
            BlockBody::Paragraph { inlines: items } => BlockBody::Paragraph {
                inlines: inlines(items),
            },
            BlockBody::List { ordered, items } => BlockBody::List {
                ordered: *ordered,
                items: items.iter().map(|item| inlines(item)).collect(),
            },
            BlockBody::Quote { blocks } => BlockBody::Quote {
                blocks: blocks.iter().map(safe_block).collect(),
            },
            BlockBody::Code { text } => BlockBody::Code {
                text: text_safe(text),
            },
            BlockBody::Image { alt, src } => BlockBody::Image {
                alt: text_safe(alt),
                src: src.clone(),
            },
        };
        fell_core::compose::Block {
            source: block.source,
            body,
        }
    }
    StyledDoc {
        blocks: doc.blocks.iter().map(safe_block).collect(),
    }
}

/// Body text without bidi controls and invisible format characters, which
/// could reorder what a line reads as. Line breaks and tabs stay.
fn text_safe(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            line.split('\t')
                .map(display_safe)
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_doc(doc: &StyledDoc, folder: &Path, paint: Paint, colors: Palette) -> gpui::Div {
    let mut column = div().flex().flex_col();
    let mut previous = None;
    for block in &doc.blocks {
        let mut painted = render_block(block, folder, paint, colors);
        if previous.is_some() && !joins_previous(previous, Some(&block.body)) {
            painted = if matches!(block.body, BlockBody::Heading { .. }) {
                painted.mt_4()
            } else {
                painted.mt_2()
            };
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

/// Where a link goes, muted after its label.
/// How styled blocks paint: how links show, and the body text size that
/// headings and list markers scale from.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Paint {
    links: Links,
    text_px: f32,
}

fn render_block(
    block: &fell_core::compose::Block,
    folder: &Path,
    paint: Paint,
    colors: Palette,
) -> gpui::Div {
    let links = paint.links;
    match &block.body {
        // Scale heading levels from the current body text.
        BlockBody::Heading { level, inlines } => div()
            .font_weight(FontWeight(680.))
            .text_size(px(paint.text_px * if *level <= 1 { 1.75 } else { 1.375 }))
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
                                .text_size(px(paint.text_px * 1.125))
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
                quote = quote.child(render_block(block, folder, paint, colors));
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
    inline_pieces(inlines, links, colors).into_row(div().flex().flex_row().flex_wrap(), colors)
}

fn inline_pieces(inlines: &[fell_core::compose::Inline], links: Links, colors: Palette) -> Pieces {
    let mut pieces = Pieces::default();
    for inline in inlines {
        match inline {
            fell_core::compose::Inline::Text(text) => {
                pieces.text(text, Piece::Words);
            }
            fell_core::compose::Inline::Strong(inner) => pieces.drawn(
                div()
                    .font_weight(FontWeight(700.))
                    .font_family("Liberation Sans")
                    .child(render_inlines(inner, links, colors)),
            ),
            fell_core::compose::Inline::Em(inner) => {
                pieces.drawn(div().italic().child(render_inlines(inner, links, colors)));
            }
            fell_core::compose::Inline::Code(text) => pieces.drawn(
                div()
                    .font_family("monospace")
                    .bg(rgb(colors.code))
                    .px_1()
                    .rounded_sm()
                    .child(text.clone()),
            ),
            fell_core::compose::Inline::Link { text, href } => {
                pieces.push(Piece::Link(text.clone()));
                if let Some(target) = shown_target(text, href, links) {
                    pieces.note(target, colors.muted);
                }
            }
            fell_core::compose::Inline::Image { alt, .. } => {
                pieces.drawn(div().child(alt.clone()));
            }
        }
    }
    pieces
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
    block.child(div().text_color(rgb(colors.dim)).child(alt.to_string()))
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
    // Surface omitted files even while the forwarded quote is folded.
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
        .bg(rgb(colors.panel))
        .text_color(rgb(colors.fg))
        .border_t_1()
        .border_color(rgb(colors.rule))
        .child(
            div()
                .min_w(px(0.))
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

/// Outcomes are kept in memory for each draft, including sends whose editor
/// was closed or switched to another draft before SMTP answered.
#[derive(Default)]
struct SendAttempts {
    next: u64,
    by_draft: HashMap<String, (u64, Option<SendFailure>)>,
}

impl gpui::Global for SendAttempts {}

impl SendAttempts {
    fn start(&mut self, id: &str) -> u64 {
        self.next += 1;
        self.by_draft.insert(id.to_string(), (self.next, None));
        self.next
    }

    fn finish(&mut self, id: &str, generation: u64, failure: Option<SendFailure>) -> bool {
        let Some((current, seen)) = self.by_draft.get_mut(id) else {
            return false;
        };
        if *current != generation {
            return false;
        }
        if failure.is_some() {
            *seen = failure;
        } else {
            self.clear(id);
        }
        true
    }

    fn failure(&self, id: &str) -> Option<&SendFailure> {
        self.by_draft
            .get(id)
            .and_then(|(_, failure)| failure.as_ref())
    }

    fn clear(&mut self, id: &str) {
        self.by_draft.remove(id);
    }
}

#[derive(Default)]
struct SendFeedbackState {
    /// Changes with the draft or an explicit send, invalidating older reads.
    generation: u64,
    readiness: Option<AccountConfigError>,
}

impl SendFeedbackState {
    fn invalidate_readiness(&mut self) {
        self.generation += 1;
    }

    fn switch_draft(&mut self) {
        self.invalidate_readiness();
        self.readiness = None;
    }

    fn update_readiness(&mut self, generation: u64, readiness: Option<AccountConfigError>) -> bool {
        if generation != self.generation || self.readiness == readiness {
            return false;
        }
        self.readiness = readiness;
        true
    }

    fn feedback<'a>(
        &'a self,
        lock: Option<SendLock>,
        attempt: Option<&'a SendFailure>,
    ) -> Option<SendFeedback<'a>> {
        match (lock, attempt) {
            (Some(SendLock::Sent), _) => Some(SendFeedback::Confirmed),
            (Some(SendLock::MaybeSent), Some(SendFailure::MaybeSent(detail))) => {
                Some(SendFeedback::Uncertain(Some(detail)))
            }
            (Some(SendLock::MaybeSent), _) => Some(SendFeedback::Uncertain(None)),
            (None, Some(SendFailure::Unavailable(error))) => Some(SendFeedback::Unavailable {
                error,
                attempted: true,
            }),
            (None, Some(SendFailure::NotSent(detail))) => Some(SendFeedback::NotSent(detail)),
            (None, Some(SendFailure::CredentialsRejected(detail))) => {
                Some(SendFeedback::CredentialsRejected(detail))
            }
            (None, _) => self
                .readiness
                .as_ref()
                .map(|error| SendFeedback::Unavailable {
                    error,
                    attempted: false,
                }),
            _ => None,
        }
    }
}

enum SendFeedback<'a> {
    Unavailable {
        error: &'a AccountConfigError,
        attempted: bool,
    },
    NotSent(&'a str),
    CredentialsRejected(&'a str),
    Uncertain(Option<&'a str>),
    Confirmed,
}

impl<'a> SendFeedback<'a> {
    fn detail(&self) -> Option<&'a str> {
        match self {
            Self::Unavailable { error, .. } => Some(&error.detail),
            Self::NotSent(detail) | Self::CredentialsRejected(detail) => Some(detail),
            Self::Uncertain(detail) => *detail,
            Self::Confirmed => None,
        }
    }
}

/// Delivery feedback wraps independently of the editor's one-line key hints.
fn send_feedback_view(
    feedback: SendFeedback<'_>,
    state: SendPanel,
    scroll: &gpui::ScrollHandle,
    colors: Palette,
) -> gpui::Div {
    let (title, guidance, sent_help, actions) = match (&feedback, state) {
        (_, SendPanel::ConfirmUnlock) => (
            "Unlock this draft?",
            "Sending again may deliver a duplicate. Unlocking only restores editing and Send.",
            None,
            "y unlock  ·  Esc cancel",
        ),
        (
            SendFeedback::Unavailable {
                error,
                attempted: true,
            },
            _,
        ) => ("Not sent", error.recovery, None, ":details  ·  :dismiss"),
        (
            SendFeedback::Unavailable {
                error,
                attempted: false,
            },
            _,
        ) => ("Send unavailable", error.recovery, None, ":details"),
        (SendFeedback::NotSent(_), _) => (
            "Not sent",
            "Edit the draft and try again.",
            None,
            ":details  ·  :dismiss",
        ),
        (SendFeedback::CredentialsRejected(_), _) => (
            "Not sent · SMTP sign-in failed",
            SMTP_AUTH_RECOVERY,
            None,
            ":details  ·  :dismiss",
        ),
        (SendFeedback::Uncertain(detail), _) => (
            "Delivery uncertain",
            "Close with :q, open Sent with g s, then sync with Space s.",
            Some("A matching copy confirms the send. Until then, delivery is unknown."),
            if detail.is_some() {
                ":details  ·  :unlock"
            } else {
                ":unlock"
            },
        ),
        (SendFeedback::Confirmed, _) => (
            "Send confirmed",
            "This draft is read-only. Open Sent with g s in the inbox.",
            None,
            ":q close",
        ),
    };
    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .gap_1()
        .px_4()
        .py_2()
        .border_t_1()
        .border_color(rgb(colors.rule))
        .bg(rgb(colors.panel))
        .text_size(px(14.))
        .child(
            div()
                .text_color(rgb(if matches!(feedback, SendFeedback::Confirmed) {
                    colors.accent
                } else {
                    colors.error
                }))
                .font_weight(FontWeight(600.))
                .child(title),
        )
        .child(guidance)
        .children(sent_help.map(|line| div().child(line)))
        .children(if state == SendPanel::Details {
            feedback.detail().map(|detail| {
                div()
                    .id("send-details")
                    .max_h(px(170.))
                    .overflow_y_scroll()
                    .track_scroll(scroll)
                    .text_color(rgb(colors.fg))
                    .child(
                        detail
                            .lines()
                            .map(display_safe)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
            })
        } else {
            None
        })
        .child(
            div()
                .text_color(rgb(colors.accent))
                .child(if state == SendPanel::Details {
                    "Esc close  ·  j/k scroll"
                } else {
                    actions
                }),
        )
}

/// Why send is blocked, as the status bar says it. A new draft has nobody
/// to send to yet: that is a block only once the person tries to send it.
fn shown_send_block(snap: &Snapshot, send_tried: bool) -> Option<&String> {
    snap.send_block
        .as_ref()
        .filter(|reason| send_tried || reason.as_str() != NO_RECIPIENT)
}

/// Whether the status line already gives the reason send is blocked, as a
/// failed send does.
fn repeats_block(status: &str, reason: &str) -> bool {
    !idle_status(status) && status.contains(reason)
}

/// A status that says nothing the mode word does not.
fn idle_status(status: &str) -> bool {
    matches!(status, "" | "normal" | "insert")
}

struct ComposeStatus<'a> {
    mode: &'a str,
    note: &'a str,
    draft_save: Option<(DraftSaveStatus, f32)>,
}

/// One line, in the inbox's style: mode and hints on the left, send on
/// the right, `key action` pairs separated by ` · `, in the mono face.
/// `measure` gives a text's width at a size, in the bar's face, so a long
/// message is cut at a word with `…` and keeps 16px clear of the right.
type StatusMeasure<'a> = &'a dyn Fn(&str, f32) -> f32;

fn status_bar(
    status: ComposeStatus<'_>,
    snap: &Snapshot,
    send_tried: bool,
    colors: Palette,
    width: f32,
    measure: StatusMeasure<'_>,
    (warning_font, measure_warning): (&str, StatusMeasure<'_>),
) -> gpui::Div {
    let wide = width > 640.;
    let size = if wide { 14. } else { 13. };
    let fresh = snap
        .to
        .iter()
        .chain(&snap.cc)
        .chain(&snap.bcc)
        .filter(|item| item.fresh)
        .count();
    let ComposeStatus {
        mode,
        note,
        draft_save,
    } = status;
    let idle = idle_status(note);
    let saved = draft_save.filter(|_| idle);
    // The header tags each first-time address; the count only fills an
    // idle bar, so it never crowds out a message or reads as a count of
    // edits. Narrow: short enough to sit beside the mode and send.
    let fresh_label = match fresh {
        _ if !idle || saved.is_some() || snap.send_block.as_deref() == Some(SEND_UNAVAILABLE) => {
            None
        }
        0 => None,
        count if !wide => Some(format!("first-time: {count}")),
        1 => Some("1 first-time recipient".to_string()),
        count => Some(format!("{count} first-time recipients")),
    };
    let right = if saved.is_some_and(|(status, _)| status.warning().is_some()) && !wide {
        Vec::new()
    } else {
        status_right(snap, note, send_tried, wide)
    };
    let gap = 8.;
    let dot_px = measure("·", size);
    let parts_px = |parts: &[(String, Tone, bool)]| -> f32 {
        let texts: f32 = parts.iter().map(|(text, _, _)| measure(text, size)).sum();
        let joins = parts.len().saturating_sub(1) as f32;
        texts + joins * (dot_px + 2. * gap)
    };
    // The room the message has: the row, less its padding, the right
    // group, the 16px between the groups, the mode and its dot, and a
    // little for the shaped width running under the painted.
    let mode_px = if mode.is_empty() {
        0.
    } else {
        measure(mode, size) + dot_px + 2. * gap
    };
    let room = width - 32. - parts_px(&right) - 16. - mode_px - 4.;
    let (message, color, opacity) = match saved {
        Some((DraftSaveStatus::Local, alpha)) => ("saved", colors.dim, alpha),
        Some((DraftSaveStatus::Server, alpha)) => ("saved to Drafts", colors.dim, alpha),
        Some((status, alpha)) => (status.warning().unwrap_or_default(), colors.error, alpha),
        None if idle => ("", colors.fg, 1.),
        None => (note, colors.fg, 1.),
    };
    let warning = saved.is_some_and(|(status, _)| status.warning().is_some());
    let message_measure = if warning { measure_warning } else { measure };
    let message = fit_words(message, room, &|text| message_measure(text, size));
    let dot = || div().text_color(rgb(colors.dim)).child("·");
    let mut left = div()
        .flex()
        .flex_1()
        .gap_2()
        .min_w(px(0.))
        .overflow_hidden();
    if !mode.is_empty() {
        left = left.child(
            div()
                .flex_shrink_0()
                .text_color(rgb(colors.dim))
                .child(mode.to_string()),
        );
    }
    if !message.is_empty() {
        if !mode.is_empty() {
            left = left.child(dot().opacity(opacity));
        }
        left = left.child(
            div()
                .min_w(px(0.))
                .whitespace_nowrap()
                .overflow_hidden()
                .text_color(rgb(color))
                .when(warning, |message| {
                    message.font_family(warning_font.to_owned())
                })
                .opacity(opacity)
                .child(message),
        );
    }
    if let Some(label) = fresh_label {
        if !mode.is_empty() {
            left = left.child(dot());
        }
        left = left.child(
            div()
                .flex_shrink_0()
                .text_color(rgb(colors.fg))
                .child(label),
        );
    }
    let mut right_group = div().flex().flex_shrink_0().gap_2();
    for (index, (text, tone, bold)) in right.into_iter().enumerate() {
        if index > 0 {
            right_group = right_group.child(dot());
        }
        let part = div().text_color(rgb(tone.of(colors))).child(text);
        right_group = right_group.child(if bold {
            part.font_weight(FontWeight(600.))
        } else {
            part
        });
    }
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_x(px(16.))
        .px(px(16.))
        .py(px(6.))
        .bg(rgb(colors.panel))
        .border_t_1()
        .border_color(rgb(colors.rule))
        .text_size(px(size))
        .child(left)
        .child(right_group)
}

/// Which palette colour a status part takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Fg,
    Dim,
    Accent,
}

impl Tone {
    fn of(self, colors: Palette) -> u32 {
        match self {
            Tone::Fg => colors.fg,
            Tone::Dim => colors.dim,
            Tone::Accent => colors.accent,
        }
    }
}

/// The status bar's right group, as (text, colour, bold) parts joined by
/// dots: send and the unreviewed count, or why send is blocked.
fn status_right(
    snap: &Snapshot,
    status: &str,
    send_tried: bool,
    wide: bool,
) -> Vec<(String, Tone, bool)> {
    match shown_send_block(snap, send_tried) {
        // The message on the left already says why: one message at a time.
        Some(reason) if repeats_block(status, reason) => Vec::new(),
        // The confirmation panel already says the draft went out.
        Some(_) if snap.send_lock_reason() == Some(SendLock::Sent.reason()) => Vec::new(),
        Some(reason) if reason.as_str() == SendLock::InFlight.reason() => {
            vec![("Sending…".into(), Tone::Fg, false)]
        }
        // The delivery panel already supplies the outcome and recovery.
        Some(reason) if reason == SendLock::MaybeSent.reason() || reason == SEND_UNAVAILABLE => {
            Vec::new()
        }
        // Narrow: the reason alone says send is blocked, and leaves the
        // message on the left its room. A missing recipient says so in two
        // words, so "send blocked" stays and reads as a block, not advice.
        Some(reason) => {
            let mut parts = Vec::new();
            if wide || reason.as_str() == NO_RECIPIENT {
                parts.push(("send blocked".into(), Tone::Dim, false));
            }
            parts.push((
                if !wide && reason.as_str() == NO_RECIPIENT {
                    "no recipient".to_string()
                } else {
                    reason.clone()
                },
                Tone::Fg,
                false,
            ));
            parts
        }
        None => {
            let mut parts = vec![("space m s send".to_string(), Tone::Fg, false)];
            if snap.unreviewed > 0 {
                // Sending accepts these, so they stand out.
                parts.push((
                    format!("{} unreviewed", snap.unreviewed),
                    Tone::Accent,
                    true,
                ));
            }
            parts
        }
    }
}

/// `text` broken into lines no wider than `max_px`, at spaces, with no
/// space kept at either end of a line. A word wider than a line is broken
/// inside itself.
fn break_lines(text: &str, max_px: f32, measure: &dyn Fn(&str) -> f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let joined = if line.is_empty() {
            word.to_string()
        } else {
            format!("{line} {word}")
        };
        if measure(&joined) <= max_px {
            line = joined;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        // A word too wide for a line of its own: as many characters as fit
        // on each line, at least one.
        let mut rest = word;
        while measure(rest) > max_px {
            let chars: Vec<(usize, char)> = rest.char_indices().collect();
            let cut = (1..chars.len())
                .rev()
                .map(|count| chars[count].0)
                .find(|at| measure(&rest[..*at]) <= max_px)
                .unwrap_or_else(|| chars.get(1).map_or(rest.len(), |(at, _)| *at));
            lines.push(rest[..cut].to_string());
            rest = &rest[cut..];
            if rest.is_empty() {
                break;
            }
        }
        line = rest.to_string();
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// `text` cut to `max_px` at a word, with `…`: never mid-glyph, never
/// ending on a separator. A single word too long for the room is cut
/// inside itself.
fn fit_words(text: &str, max_px: f32, measure: &dyn Fn(&str) -> f32) -> String {
    if measure(text) <= max_px {
        return text.to_string();
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    for keep in (1..words.len()).rev() {
        let mut kept = words[..keep].to_vec();
        while kept
            .last()
            .is_some_and(|word| !word.chars().any(char::is_alphanumeric))
        {
            kept.pop();
        }
        if kept.is_empty() {
            continue;
        }
        let joined = kept.join(" ");
        // A word already cut ("is…”") says the text goes on.
        let candidate = if joined.trim_end_matches(['\u{201d}', '"']).ends_with('…') {
            joined
        } else {
            format!("{joined}…")
        };
        if measure(&candidate) <= max_px {
            return candidate;
        }
    }
    fit_px(text, max_px, measure)
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
                .child("d discard   s send   p preview   r raw   q quote   t theme"),
        )
        .child("space a  agent edit")
        .child(div().text_color(rgb(colors.dim)).child("k keep   x reject"))
        .child("space ?  help")
        .child(div().text_color(rgb(colors.dim)).child("l diagnostic log"))
}

const DISCARD_DRAFT_LABEL: &str = "Discard draft";

fn palette_view(
    query: &str,
    commands: &'static str,
    can_discard: bool,
    colors: Palette,
) -> gpui::Div {
    let discard_selected = palette_action(query) == Some("discard");
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
        .child(div().text_sm().text_color(rgb(colors.dim)).child(commands))
        .when(
            can_discard && (query.trim().is_empty() || discard_selected),
            |panel| {
                panel.child(
                    div()
                        .mt_2()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .text_sm()
                        .when(discard_selected, |row| row.bg(rgb(colors.selection)))
                        .child(DISCARD_DRAFT_LABEL)
                        .child(div().text_color(rgb(colors.dim)).child("Space m d")),
                )
            },
        )
}

/// A scrim of page colour over the whole window, so the draft behind the
/// HTML preview recedes. The panel itself paints above it.
fn preview_scrim(colors: Palette) -> gpui::Div {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right_0()
        .bg(rgba((colors.bg << 8) | SCRIM_ALPHA))
}

/// The HTML preview before send. It covers the fields and the body, from
/// under the header to the Attachments divider, 12px in from the sides and
/// the bottom, so no cut-off words or half rows show around it. Deferred,
/// so it paints over the scrim that dims the rest of the window.
fn preview_panel(snap: &Snapshot, colors: Palette, text_px: f32) -> impl IntoElement {
    let doc = markdown_to_doc(&snap.body);
    deferred(
        div()
            .absolute()
            .top(px(PREVIEW_INSET.top))
            .bottom(px(PREVIEW_INSET.side))
            .left(px(PREVIEW_INSET.side))
            .right(px(PREVIEW_INSET.side))
            .bg(rgb(colors.bg))
            .border_1()
            .border_color(rgb(colors.accent))
            .id("html-preview")
            .p_4()
            .overflow_y_scroll()
            // 4px over the line leading: an 8px gap to the first line.
            .child(
                div()
                    .mb_1()
                    .text_sm()
                    .text_color(rgb(colors.dim))
                    .child("HTML preview"),
            )
            .child(render_doc(
                &doc,
                Path::new(""),
                Paint {
                    links: Links::Target,
                    text_px,
                },
                colors,
            )),
    )
}

/// Where the preview panel sits inside the area it covers: just under the
/// compose header, and 12px in from each side and from the Attachments
/// divider.
struct PreviewInset {
    top: f32,
    side: f32,
}

const PREVIEW_INSET: PreviewInset = PreviewInset { top: 2., side: 12. };

/// How much of the page colour the scrim behind an overlay lays down.
const SCRIM_ALPHA: u32 = 0x99;

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
        reply_to_unverified: false,
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
    if href.is_empty() || href == text.trim() && idn_place(href).is_none() {
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
    match links {
        Links::Target => link_target(text, href),
        Links::Host | Links::Label => {
            let mut places = text
                .split_whitespace()
                .filter_map(|word| place_of_word(word, links))
                .peekable();
            (idn_place(href).is_some()
                || (links == Links::Host && places.peek().is_none())
                || label_misleads(places, href))
            .then(|| format!("({})", link_host(href)))
        }
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

/// The whole target, an IDN host or mail domain in `xn--` form.
fn ascii_target(href: &str) -> String {
    let href = href.trim();
    if href.is_ascii() {
        return href.to_string();
    }
    let Ok(url) = url::Url::parse(href) else {
        return href.to_string();
    };
    match mail_addresses(&url) {
        // A mail address has no host to the URL parser, so its domain
        // would show percent-encoded: it shows as the address it is.
        Some(addresses) => {
            let query = url.query().map(|query| format!("?{query}"));
            format!(
                "mailto:{}{}",
                addresses.join(","),
                query.unwrap_or_default()
            )
        }
        None => url.to_string(),
    }
}

/// The `xn--` form of where a link goes, when it is not plain ASCII: the
/// host, or a `mailto:` address, whose domain is a host too.
fn idn_place(href: &str) -> Option<String> {
    let url = url::Url::parse(href.trim()).ok()?;
    let idn = |host: &str| host.split('.').any(|label| label.starts_with("xn--"));
    if let Some(addresses) = mail_addresses(&url) {
        return addresses
            .iter()
            .any(|address| {
                address
                    .rsplit_once('@')
                    .is_some_and(|(_, domain)| idn(domain))
            })
            .then(|| addresses.join(","));
    }
    let host = url.host_str()?;
    idn(host).then(|| host.to_string())
}

/// The addresses of a `mailto:` link, each domain in lowercase `xn--`
/// form. `None` for any other link.
fn mail_addresses(url: &url::Url) -> Option<Vec<String>> {
    if !url.scheme().eq_ignore_ascii_case("mailto") {
        return None;
    }
    let addresses = url
        .path()
        .split(',')
        .filter_map(|one| {
            let (local, domain) = one.trim().rsplit_once('@')?;
            Some(format!(
                "{}@{}",
                local.to_lowercase(),
                ascii_domain(domain)?
            ))
        })
        .collect::<Vec<_>>();
    (!addresses.is_empty()).then_some(addresses)
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

/// The ASCII (`xn--`) lowercase form of a domain, without the root's
/// trailing dot. A percent-encoded name decodes first.
fn ascii_domain(host: &str) -> Option<String> {
    let url = url::Url::parse(&format!("http://{host}/")).ok()?;
    let host = url.host_str()?;
    Some(host.strip_suffix('.').unwrap_or(host).to_string())
}

/// The ASCII (`xn--`) lowercase form of a host, without `www.`.
fn ascii_host(host: &str) -> Option<String> {
    let host = ascii_domain(host)?;
    Some(host.strip_prefix("www.").unwrap_or(&host).to_string())
}

fn place_of_href(href: &str) -> Option<Place> {
    let url = url::Url::parse(href.trim()).ok()?;
    if let Some(addresses) = mail_addresses(&url) {
        // More than one address reads as none of them alone.
        let (_, domain) = addresses.last()?.rsplit_once('@')?;
        return Some(Place::Mail {
            domain: domain.strip_prefix("www.").unwrap_or(domain).to_string(),
            address: addresses.join(","),
        });
    }
    if url.scheme().eq_ignore_ascii_case("mailto") {
        return None;
    }
    ascii_host(url.host_str()?).map(Place::Host)
}

/// The place one word of a label reads as, if it reads as one at all:
/// a scheme (`https://…`), an address with `@`, or a dotted name ending
/// in letters. In the user's draft a bare IP can also name a host.
fn place_of_word(word: &str, links: Links) -> Option<Place> {
    let word_char =
        |ch: char| ch.is_alphanumeric() || matches!(ch, '/' | '-' | '_' | '@' | ':' | '%' | '~');
    if links == Links::Label {
        // Keep IPv6 brackets long enough to distinguish an optional port.
        let literal = word.trim_matches(|ch| !(word_char(ch) || matches!(ch, '[' | ']')));
        if let Some(host) = literal_ip_host(literal) {
            return Some(Place::Host(host));
        }
    }
    let word = word.trim_matches(|ch| !word_char(ch));
    let lower = word.to_lowercase();
    let lower = lower.strip_prefix("mailto:").unwrap_or(&lower);
    if lower.contains("://") {
        let url = url::Url::parse(lower).ok()?;
        return url.host_str().and_then(ascii_host).map(Place::Host);
    }
    if let Some((local, domain)) = lower.rsplit_once('@')
        && !local.is_empty()
    {
        let domain = dotted_name(domain.strip_suffix('.').unwrap_or(domain))?;
        let address = format!("{local}@{}", ascii_domain(domain)?);
        return Some(Place::Mail {
            address,
            domain: ascii_host(domain)?,
        });
    }
    let name = lower.split(['/', '?', '#']).next()?;
    let name = without_port(name);
    // `paypal.com./login` names paypal.com: the root's dot is no part of it.
    let name = name.strip_suffix('.').unwrap_or(name);
    ascii_host(dotted_name(name)?).map(Place::Host)
}

fn without_port(name: &str) -> &str {
    name.rsplit_once(':').map_or(name, |(host, port)| {
        if port.chars().all(|ch| ch.is_ascii_digit()) {
            host
        } else {
            name
        }
    })
}

/// A bare IP in the same form as a URL host.
fn literal_ip_host(word: &str) -> Option<String> {
    let name = word.split(['/', '?', '#']).next()?;
    // For IPv6 without brackets, every colon belongs to the address.
    if let Ok(ip) = name.parse::<Ipv6Addr>() {
        return Some(url::Host::<String>::Ipv6(ip).to_string());
    }
    let name = without_port(name);
    if !name.starts_with('[')
        && !name
            .chars()
            .next()
            .is_some_and(|ch| ch.is_numeric() || ch == '%')
    {
        return None;
    }
    match url::Host::parse(name).ok()? {
        url::Host::Ipv4(ip) => {
            let label = name.strip_suffix('.').unwrap_or(name);
            let mut fields = label.rsplit('.');
            let tail = fields.next()?;
            // A short decimal or version label is ordinary text. Retain
            // shorthand when the last field actually packs multiple octets,
            // and let the URL parser normalize hexadecimal and escapes.
            let ordinary_number = label.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                && match fields.count() {
                    0 => ip.octets()[0] == 0,
                    1 | 2 => tail.parse::<u8>().is_ok(),
                    _ => false,
                };
            (!ordinary_number).then(|| ip.to_string())
        }
        ip @ url::Host::Ipv6(_) => Some(ip.to_string()),
        url::Host::Domain(_) => None,
    }
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

/// Whether a word of the label reads as an address that is not where the
/// link goes: "Sign in at paypal.com" to another host.
fn label_misleads(mut places: impl Iterator<Item = Place>, href: &str) -> bool {
    let target = place_of_href(href);
    places.any(|place| match (&target, &place) {
        (None, _) => true,
        (Some(Place::Mail { address, .. }), Place::Mail { address: word, .. }) => address != word,
        (Some(target), place) => target.domain() != place.domain(),
    })
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
    // Vim's `:q`. `quote` still runs from `:qu`.
    if query == "q" || query == "quit" {
        return Some("close");
    }
    // Spelled out in full: it lets a draft that may have gone out be sent
    // again.
    if query == "unlock" {
        return Some("unlock");
    }
    match query.as_str() {
        "complete recipient" | "complete" => return Some("complete recipient"),
        "details" => return Some("details"),
        "dismiss" => return Some("dismiss"),
        "diagnostic log" | "open diagnostic log" => return Some("diagnostic log"),
        _ => {}
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
    found.or_else(|| {
        (query == "d"
            || (query.len() > 1 && crate::keymap::fuzzy(&query, DISCARD_DRAFT_LABEL).is_some()))
        .then_some("discard")
    })
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
    fn an_uncertain_send_uses_the_feedback_panel_after_reopening() {
        use super::{SendFailure, SendFeedback, SendFeedbackState, SendLock};
        let state = SendFeedbackState::default();
        let lock = Some(SendLock::MaybeSent);
        let failure = SendFailure::MaybeSent("SMTP connection closed".into());
        assert_eq!(
            state.feedback(lock, Some(&failure)).unwrap().detail(),
            Some("SMTP connection closed"),
        );
        assert!(matches!(
            state.feedback(Some(SendLock::Sent), Some(&failure)),
            Some(SendFeedback::Confirmed),
        ));
        assert!(matches!(
            state.feedback(lock, None),
            Some(SendFeedback::Uncertain(None)),
        ));
        let mut snap = super::empty_snapshot("d");
        snap.send_block = Some(SendLock::MaybeSent.reason().into());
        for wide in [true, false] {
            assert!(super::status_right(&snap, "", false, wide).is_empty());
        }
    }

    #[test]
    fn stale_readiness_cannot_replace_a_confirmed_attempts_smtp_rejection() {
        use super::{
            AccountConfigError, SendAttempts, SendFailure, SendFeedback, SendFeedbackState,
        };
        let missing = AccountConfigError {
            detail: "account.toml does not exist".into(),
            recovery: "Run fell account setup.",
        };
        let mut state = SendFeedbackState::default();
        let mut attempts = SendAttempts::default();
        let before_send = state.generation;
        assert!(state.update_readiness(before_send, Some(missing.clone())));

        // The file was restored and the user sent against the restored
        // settings, but the displayed readiness is still stale.
        state.invalidate_readiness();
        let attempt = attempts.start("draft");
        let during_send = state.generation;
        assert!(attempts.finish(
            "draft",
            attempt,
            Some(SendFailure::NotSent("554 submission refused".into()))
        ));
        state.invalidate_readiness();
        assert!(!state.update_readiness(before_send, None));
        assert!(!state.update_readiness(during_send, None));
        assert!(matches!(
            state.feedback(None, attempts.failure("draft")),
            Some(SendFeedback::NotSent("554 submission refused")),
        ));
        assert!(state.update_readiness(state.generation, None));
        assert_eq!(
            state
                .feedback(None, attempts.failure("draft"))
                .unwrap()
                .detail(),
            Some("554 submission refused")
        );
    }

    #[test]
    fn a_send_time_config_failure_survives_restored_readiness_until_the_next_send() {
        use super::{
            AccountConfigError, SendAttempts, SendFailure, SendFeedback, SendFeedbackState,
        };
        let missing = AccountConfigError {
            detail: "account.toml does not exist".into(),
            recovery: "Run fell account setup.",
        };
        let mut state = SendFeedbackState::default();
        let mut attempts = SendAttempts::default();
        state.invalidate_readiness();
        let attempt = attempts.start("draft");
        assert!(attempts.finish(
            "draft",
            attempt,
            Some(SendFailure::Unavailable(missing.clone()))
        ));
        state.invalidate_readiness();
        assert!(state.update_readiness(state.generation, Some(missing.clone())));
        assert!(state.update_readiness(state.generation, None));
        assert!(matches!(
            state.feedback(None, attempts.failure("draft")),
            Some(SendFeedback::Unavailable { error, attempted: true }) if error == &missing,
        ));
        let before_switch = state.generation;
        state.switch_draft();
        assert!(!state.update_readiness(before_switch, Some(missing.clone())));
        assert!(
            state
                .feedback(None, attempts.failure("other draft"))
                .is_none()
        );
        assert_eq!(
            state
                .feedback(None, attempts.failure("draft"))
                .unwrap()
                .detail(),
            Some(missing.detail.as_str())
        );

        attempts.start("draft");
        assert!(!attempts.finish("draft", attempt, Some(SendFailure::Unavailable(missing))));
        assert!(state.feedback(None, attempts.failure("draft")).is_none());
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
            super::edit_preview_words(&snap, &snap.hunks[0], 4).as_deref(),
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
            // A link's URL and where it goes take the body colour on an
            // agent edit's tint, focused or not.
            let tints = [colors.hunk, colors.hunk_focus];
            for bg in [colors.bg, colors.panel, colors.code]
                .into_iter()
                .chain(tints)
            {
                assert!(
                    contrast(colors.fg, bg) >= TEXT_CONTRAST,
                    "{name} fg on {bg:06x}"
                );
            }
            for bg in [colors.bg, colors.panel].into_iter().chain(tints) {
                assert!(
                    contrast(colors.dim, bg) >= TEXT_CONTRAST,
                    "{name} dim on {bg:06x}"
                );
            }
            // The link text on a tint.
            for bg in [colors.bg, colors.panel].into_iter().chain(tints) {
                assert!(
                    contrast(colors.accent, bg) >= TEXT_CONTRAST,
                    "{name} accent on {bg:06x}"
                );
            }
            // Where a link goes, off the tints: AA, and quieter than the
            // prose, so it reads as an aside.
            for bg in [colors.bg, colors.panel, colors.code] {
                assert!(
                    contrast(colors.muted, bg) >= TEXT_CONTRAST,
                    "{name} muted on {bg:06x}"
                );
            }
            assert!(
                contrast(colors.muted, colors.bg) < contrast(colors.fg, colors.bg),
                "{name} muted"
            );
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
        assert_eq!(super::palette_action("d"), Some("discard"));
        assert_eq!(super::palette_action("draft"), Some("discard"));
        assert_eq!(super::palette_action("dscrd"), Some("discard"));
        assert_eq!(super::palette_action("a"), None);
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
        assert_eq!(super::palette_action("details"), Some("details"));
        assert_eq!(super::palette_action("dismiss"), Some("dismiss"));
        assert_eq!(super::palette_action("discard"), Some("discard"));
        assert_eq!(super::palette_action("Discard draft"), Some("discard"));
        assert_eq!(super::palette_action("unl"), None);
        assert_eq!(super::palette_action("unlock"), Some("unlock"));
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
        fell_core::compose::DraftSession::testing_create_local(folder, tag, account, fields, body)
            .unwrap()
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
    /// note after it with a space, as `raw_line` paints it.
    fn shown(line: &str, show_markers: bool, urls: super::Urls) -> String {
        let colors = super::palette(super::ThemeKind::Dark);
        let pieces = super::source_pieces(
            line,
            None,
            show_markers,
            urls,
            super::Mode::Normal,
            false,
            colors,
        );
        pieces.0.iter().map(piece_text).collect()
    }

    /// The words of one piece of a row; a drawn marker shows as nothing.
    fn piece_text(piece: &super::Piece) -> String {
        match piece {
            super::Piece::Span { text, .. }
            | super::Piece::Glyph { text, .. }
            | super::Piece::Words(text)
            | super::Piece::Link(text) => text.clone(),
            super::Piece::Caret(_) | super::Piece::Drawn(_) => String::new(),
            super::Piece::Spaced(piece, spaces) => format!("{}{spaces}", piece_text(piece)),
        }
    }

    /// The colour each run of text is painted in.
    fn piece_colors(pieces: &super::Pieces) -> Vec<(String, u32)> {
        fn walk(piece: &super::Piece, out: &mut Vec<(String, u32)>) {
            match piece {
                super::Piece::Span { text, color, .. } => out.push((text.clone(), *color)),
                super::Piece::Spaced(piece, _) => walk(piece, out),
                _ => {}
            }
        }
        let mut out = Vec::new();
        for piece in &pieces.0 {
            walk(piece, &mut out);
        }
        out
    }

    #[test]
    fn both_brackets_of_a_link_take_the_marker_colour() {
        use super::{Mode, Urls};
        // Round 4: on the cursor line "[" was the marker colour and "]"
        // the URL's.
        let line = "The [March plan](https://files.example/plan) is attached.";
        for kind in [super::ThemeKind::Dark, super::ThemeKind::Light] {
            let colors = super::palette(kind);
            for tinted in [false, true] {
                let pieces = super::source_pieces(
                    line,
                    Some(0),
                    true,
                    Urls::Source,
                    Mode::Insert,
                    tinted,
                    colors,
                );
                let runs = piece_colors(&pieces);
                let color_of = |text: &str| {
                    runs.iter()
                        .find(|(run, _)| run == text)
                        .map(|(_, color)| *color)
                        .unwrap_or_else(|| panic!("no {text:?} in {runs:?}"))
                };
                assert_eq!(color_of("["), colors.marker);
                assert_eq!(color_of("]"), colors.marker);
                // The URL keeps its own, quieter colour.
                let url = if tinted { colors.fg } else { colors.muted };
                assert_eq!(color_of("(https://files.example/plan)"), url);
            }
        }
    }

    #[test]
    fn focused_link_offsets_skip_idn_notes_and_keep_utf8_source_order() {
        use super::{
            Mode, ThemeKind, focused_link_text, has_source_link, source_col, wrap_focused_link,
        };

        let line = "[café](https://bücher.test/) and [2001:db8::7](https://[2001:db8::8]/révision)";
        let styles = super::line_styles(line);
        let colors = super::palette(ThemeKind::Dark);
        let font = gpui::font("Liberation Sans");
        let at = line.find("évision").unwrap();
        let col = line[..at].chars().count();
        assert!(has_source_link(line, &styles));
        let image = "![preview](attachment.png)";
        assert!(!has_source_link(image, &super::line_styles(image)));

        let inserted = focused_link_text(line, &styles, col, Mode::Insert, false, &font, colors);
        assert_eq!(inserted.notes.len(), 1);
        let note = &inserted.notes[0];
        assert_eq!(&inserted.text[note.clone()], " (xn--bcher-kva.test)");
        for (display_byte, _) in inserted.text.char_indices() {
            let source_byte =
                display_byte - (display_byte.clamp(note.start, note.end) - note.start);
            assert_eq!(
                source_col(line, &inserted.notes, display_byte),
                line[..source_byte].chars().count()
            );
        }
        let display_at = inserted.insert_caret.unwrap();
        assert_eq!(display_at, at + note.len());
        assert_eq!(source_col(line, &inserted.notes, display_at), col);
        assert_eq!(
            source_col(line, &inserted.notes, inserted.text.len()),
            line.chars().count()
        );
        let end = Some(line.chars().count());
        assert_eq!(
            super::source_cursor_byte(line, end, Mode::Normal),
            Some(line.len() - 1)
        );
        assert_eq!(
            super::source_cursor_byte(line, end, Mode::Insert),
            Some(line.len())
        );

        let (wrapped, runs, newlines) =
            wrap_focused_link(&inserted.text, inserted.runs, &[note.start + 5, display_at]);
        assert_eq!(wrapped.replace('\n', ""), inserted.text);
        assert_eq!(runs.iter().map(|run| run.len).sum::<usize>(), wrapped.len());
        let wrapped_at = newlines[1] + 1;
        assert!(wrapped[wrapped_at..].starts_with('é'));
        assert_eq!(
            source_col(line, &inserted.notes, wrapped_at - newlines.len()),
            col
        );

        let normal = focused_link_text(line, &styles, col, Mode::Normal, false, &font, colors);
        assert_eq!(normal.text, inserted.text);
        let mut start = 0;
        let mut highlighted = false;
        for run in normal.runs {
            let end = start + run.len;
            let glyphs = &normal.text[start..end];
            if run.background_color.is_some() {
                assert!(!highlighted);
                highlighted = true;
                assert_eq!(start, display_at);
                assert_eq!(glyphs, "é");
                assert_eq!(run.background_color, Some(gpui::rgb(colors.caret).into()));
            }
            start = end;
        }
        assert!(highlighted);
        assert_eq!(start, normal.text.len());
    }

    #[test]
    fn a_row_breaks_only_after_a_space() {
        fn cut(text: &str) -> Vec<&str> {
            super::words(text).collect()
        }
        assert_eq!(cut("is attached, with"), ["is ", "attached, ", "with"]);
        assert_eq!(cut("  indented  twice "), ["  indented  ", "twice "]);
        assert_eq!(
            cut("(https://files.example/plan)"),
            ["(https://files.example/plan)"]
        );
        assert!(cut("").is_empty());
        // Nothing is dropped.
        let text = " a  b c ";
        assert_eq!(cut(text).concat(), text);
    }

    #[test]
    fn a_piece_the_row_wraps_starts_at_the_margin() {
        use super::{Links, Mode, Urls};
        // Round 4: at 460 px, " is attached." wrapped to a new line one
        // space in. The spaces between pieces stay on the line before.
        let colors = super::palette(super::ThemeKind::Dark);
        let lines = [
            "The [March plan](https://files.example/plan) is attached.",
            "Call **Ada** about `deploy` and *then* the [notes](https://notes.example/).",
            "- [files.example](https://elsewhere.example/) is   where it lives",
        ];
        let check = |pieces: &super::Pieces, what: &str| {
            for piece in pieces.0.iter().skip(1) {
                // A drawn piece (a link, a bullet) is not a space.
                let text = match piece {
                    super::Piece::Spaced(inner, spaces)
                        if matches!(**inner, super::Piece::Drawn(_)) =>
                    {
                        format!("x{spaces}")
                    }
                    piece => piece_text(piece),
                };
                assert!(
                    !text.starts_with(' '),
                    "{what}: piece {text:?} starts with a space"
                );
            }
        };
        for line in lines {
            for (cursor, markers, urls) in [
                (None, false, Urls::Fold(Links::Label)),
                (None, false, Urls::Bare),
                (Some(2), true, Urls::Source),
            ] {
                for mode in [Mode::Normal, Mode::Insert] {
                    let pieces =
                        super::source_pieces(line, cursor, markers, urls, mode, false, colors);
                    check(&pieces, line);
                    // Nothing is lost: the line reads as before.
                    if cursor.is_none() {
                        assert_eq!(
                            pieces.0.iter().map(piece_text).collect::<String>(),
                            shown(line, markers, urls)
                        );
                    }
                }
            }
            let doc = fell_core::compose::markdown_to_doc(line);
            for block in &doc.blocks {
                if let super::BlockBody::Paragraph { inlines } = &block.body {
                    check(&super::inline_pieces(inlines, Links::Target, colors), line);
                }
            }
        }
        // A line's own indent stays.
        let pieces = super::source_pieces(
            "  indented",
            None,
            false,
            Urls::Bare,
            Mode::Normal,
            false,
            colors,
        );
        assert_eq!(piece_text(&pieces.0[0]), "  indented");
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
    fn folded_ip_links_show_a_different_destination() {
        use super::{Links, Urls, inline_pieces, palette};
        let colors = palette(super::ThemeKind::Dark);
        for (label, href, host) in [
            ("192.0.2.7", "https://docs.example/notes", "docs.example"),
            (
                "2001:db8::7",
                "https://[2001:db8::8]/notes",
                "[2001:db8::8]",
            ),
        ] {
            let line = format!("[{label}]({href})");
            let expected = format!("{label} ({host})");
            assert_eq!(shown(&line, false, Urls::Fold(Links::Label)), expected);
            assert_eq!(shown(&line, true, Urls::Source), line);
            let doc = markdown_to_doc(&line);
            let super::BlockBody::Paragraph { inlines } = &doc.blocks[0].body else {
                panic!("expected a paragraph");
            };
            let styled: String = inline_pieces(inlines, Links::Label, colors)
                .0
                .iter()
                .map(piece_text)
                .collect();
            assert_eq!(styled, expected);
        }
    }

    #[test]
    fn literal_ip_labels_use_the_same_host_normalization_as_urls() {
        use super::{Links, shown_target};
        for (label, href, host) in [
            (
                "See 192.000.002.007.:8443/notes",
                "https://192.0.2.7/",
                "192.0.2.7",
            ),
            (
                "2001:0DB8:0:0:0:0:0:7",
                "https://[2001:db8::7]/",
                "[2001:db8::7]",
            ),
            (
                "See ([2001:DB8::7]:8443/notes).",
                "https://[2001:db8::7]/",
                "[2001:db8::7]",
            ),
            (
                "::ffff:192.0.2.7",
                "https://[::ffff:c000:207]/",
                "[::ffff:c000:207]",
            ),
            (
                "[::ffff:192.0.2.7]",
                "https://[::ffff:c000:207]/",
                "[::ffff:c000:207]",
            ),
            ("192%2e0%2e2%2e7", "https://192.0.2.7/", "192.0.2.7"),
            ("192%2e0.2.7", "https://192.0.2.7/", "192.0.2.7"),
            ("192.0.519", "https://192.0.2.7/", "192.0.2.7"),
            ("3221225991", "https://192.0.2.7/", "192.0.2.7"),
            ("0xc0000207", "https://192.0.2.7/", "192.0.2.7"),
        ] {
            assert_eq!(shown_target(label, href, Links::Label), None, "{label}");
            assert_eq!(
                shown_target(label, "https://docs.example/", Links::Label).as_deref(),
                Some("(docs.example)"),
                "{label}"
            );
            assert_eq!(
                shown_target(label, href, Links::Host),
                Some(format!("({host})")),
                "{label}"
            );
        }
        for label in [
            "3.5 stars",
            "42 results",
            "1.2.3 notes",
            "v1.2.3.4 notes",
            "192.0.2.999",
            "2001:db8:7",
        ] {
            assert_eq!(
                shown_target(label, "https://docs.example/", Links::Label),
                None,
                "{label}"
            );
        }
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
    fn a_mailto_look_alike_shows_its_ascii_domain_everywhere() {
        use super::{Links, Urls, ascii_target, shown_target};
        // "pаypal" with a Cyrillic "а": a mailto has no host to the URL
        // parser, so its domain was never put in `xn--` form.
        let address = "ada@p\u{430}ypal.com";
        let href = format!("mailto:{address}");
        let ascii = "ada@xn--pypal-4ve.com";
        // In the styled body, in every mode, even when the label is the
        // address itself.
        for label in ["write to Ada", address] {
            for links in [Links::Label, Links::Host] {
                assert_eq!(
                    shown_target(label, &href, links).as_deref(),
                    Some(format!("({ascii})").as_str()),
                    "{label} {links:?}"
                );
            }
            // The preview before send: the address, not percent-encoding.
            assert_eq!(
                shown_target(label, &href, Links::Target).as_deref(),
                Some(format!("(mailto:{ascii})").as_str()),
                "{label}"
            );
        }
        assert_eq!(ascii_target(&href), format!("mailto:{ascii}"));
        assert_eq!(
            ascii_target(&format!("{href}?subject=Hi")),
            format!("mailto:{ascii}?subject=Hi")
        );
        // On the cursor line, beside the address as written.
        let line = format!("[{address}]({href})");
        let raw = shown(&line, true, Urls::Source);
        assert!(
            raw.starts_with(&line) && raw.ends_with(&format!("({ascii})")),
            "{raw}"
        );
        // An ASCII address stays as it was.
        assert_eq!(
            shown_target("ada@example.com", "mailto:ada@example.com", Links::Label),
            None
        );
        assert_eq!(super::idn_place("mailto:ada@example.com"), None);
        // A second address is not hidden behind the first.
        assert_eq!(
            shown_target(
                "ada@bank.example",
                "mailto:ada@bank.example,eve@evil.example",
                Links::Label
            )
            .as_deref(),
            Some("(ada@bank.example,eve@evil.example)")
        );
    }

    #[test]
    fn a_trailing_dot_before_a_path_or_port_still_names_the_host() {
        use super::{Links, shown_target};
        for label in [
            "paypal.com./login",
            "paypal.com.:443",
            "https://paypal.com./login",
        ] {
            assert_eq!(
                shown_target(label, "https://evil.example/", Links::Label).as_deref(),
                Some("(evil.example)"),
                "{label}"
            );
            // Where it does go, the label says so: nothing to add.
            assert_eq!(
                shown_target(label, "https://paypal.com/login", Links::Label),
                None,
                "{label}"
            );
        }
        assert_eq!(
            shown_target("ada@paypal.com.", "mailto:eve@evil.example", Links::Label).as_deref(),
            Some("(eve@evil.example)")
        );
    }

    #[test]
    fn an_agent_changed_line_off_the_cursor_folds_both_brackets() {
        use super::{Links, Urls};
        // Round 3: "The March plan](https://…) is attached." kept its "]".
        let line = "The [March plan](https://files.example/plan) is attached.";
        assert_eq!(
            shown(line, false, Urls::Bare),
            "The March plan (https://files.example/plan) is attached."
        );
        // The whole target shows, even where the label names its host.
        assert_eq!(
            shown(
                "[files.example](https://files.example/plan)",
                false,
                Urls::Bare
            ),
            "files.example (https://files.example/plan)"
        );
        // A look-alike host shows in its ASCII form there too.
        let href = "https://p\u{430}ypal.com/";
        let bare = shown(&format!("[pay]({href})"), false, Urls::Bare);
        assert_eq!(bare, "pay (https://xn--pypal-4ve.com/)");
        // Off an agent's lines the label alone still shows.
        assert_eq!(
            shown(line, false, Urls::Fold(Links::Label)),
            "The March plan is attached."
        );
    }

    #[test]
    fn a_failed_send_gives_its_reason_once() {
        let reason = super::NO_RECIPIENT;
        assert!(super::repeats_block(&format!("Not sent: {reason}"), reason));
        // With nothing on the left, the bar says why send is blocked.
        assert!(!super::repeats_block("", reason));
        assert!(!super::repeats_block("normal", reason));
        assert!(!super::repeats_block("saved", reason));
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
    fn a_draft_preview_loses_bidi_controls_but_keeps_its_lines() {
        let doc = fell_core::compose::markdown_to_doc(
            "Pay \u{202e}eciovni\u{202c} [here\u{200b}](https://example.com)\nnow\n",
        );
        let safe = super::safe_doc(&doc);
        let fell_core::compose::BlockBody::Paragraph { inlines } = &safe.blocks[0].body else {
            panic!("{safe:?}");
        };
        let text: String = inlines
            .iter()
            .map(|inline| match inline {
                fell_core::compose::Inline::Text(text) => text.clone(),
                fell_core::compose::Inline::Link { text, .. } => format!("[{text}]"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(text, "Pay eciovni [here] now");
        // Breaks and tabs, which plain lines and code keep, stay.
        assert_eq!(super::text_safe("a\nb\u{202e}c\td"), "a\nbc\td");
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
    fn an_unavailable_account_uses_the_feedback_panel_instead_of_the_send_hint() {
        let mut snap = super::empty_snapshot("d");
        snap.send_block = Some(super::SEND_UNAVAILABLE.into());
        for wide in [false, true] {
            assert!(super::status_right(&snap, "insert", false, wide).is_empty());
        }
    }

    #[test]
    fn the_quoted_original_is_the_stored_quote() {
        let Some(root) = fell_core::test_support::in_isolated_process() else {
            return;
        };
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
            super::open_demo(root.join("demo"), super::ThemeKind::Dark).expect("demo draft");
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

    /// A stand-in for shaped widths: 8px a character.
    fn eight_px(text: &str) -> f32 {
        text.chars().count() as f32 * 8.0
    }

    fn one_body_edit(body: &str, edited: &str) -> super::Snapshot {
        let mut snap = super::empty_snapshot("d");
        snap.body = body.into();
        snap.text = format!("---\nto: []\n---\n{body}");
        let start = snap.text.find(edited).unwrap();
        snap.hunks = vec![fell_core::compose::HunkView {
            id: 1,
            kind: super::HunkKind::Body,
            start,
            end: start + edited.len(),
            label: String::new(),
            previous: String::new(),
        }];
        snap
    }

    #[test]
    fn the_edit_bar_quotes_plain_words_without_links() {
        assert_eq!(
            super::plain_words(
                "- The **[March plan](https://files.example/plan-v2)** is at https://files.example/x now"
            ),
            "The March plan is at now"
        );
        assert_eq!(
            super::plain_words("![logo](https://a.example/l.png) hi"),
            "logo hi"
        );
        assert_eq!(
            super::plain_words("<https://a.example> and www.b.example"),
            "and"
        );
        let line =
            "The [March plan](https://files.example/plan-v2) is attached, with the lodge booking.";
        let snap = one_body_edit(&format!("Hi Ada,\n\n{line}\n"), line);
        // The ellipsis sits on the last word, and no URL shows.
        assert_eq!(
            super::edit_preview_words(&snap, &snap.hunks[0], 4).as_deref(),
            Some("The March plan is…")
        );
    }

    #[test]
    fn the_edit_bar_is_one_line_and_its_keys_never_go() {
        let line =
            "The [March plan](https://files.example/plan-v2) is attached, with the lodge booking.";
        let mut snap = one_body_edit(&format!("Hi Ada,\n\n{line}\n\nThanks\n"), line);
        let keys = ["]h [h move", "space a k keep", "space a x reject"];
        let width = |parts: &[String]| {
            parts.iter().map(|part| eight_px(part)).sum::<f32>()
                + parts.len().saturating_sub(1) as f32 * (eight_px("·") + 16.0)
        };
        let sweep = |snap: &super::Snapshot| {
            let mut seen = Vec::new();
            for room in (100..1400).step_by(10).map(|room| room as f32) {
                let parts = super::hunk_banner_parts(snap, Some(1), room, &eight_px);
                // Whatever is shown fits the row, unless even the two
                // review keys alone do not.
                if parts.len() > 2 {
                    assert!(width(&parts) <= room, "{parts:?} in {room}");
                }
                // The keep and reject keys always show, whole and last.
                assert_eq!(&parts[parts.len() - 2..], &keys[1..], "{parts:?}");
                for part in &parts {
                    assert!(!part.contains("https://") && !part.contains("]("), "{part}");
                }
                seen.push(parts);
            }
            seen
        };
        // One edit: nothing to move between, so no move keys.
        let seen = sweep(&snap);
        assert!(
            seen.iter()
                .all(|parts| !parts.contains(&keys[0].to_string()))
        );
        assert_eq!(
            seen.last().unwrap(),
            &vec![
                "agent edit \u{201c}The March plan is attached\u{2026}\u{201d}".to_string(),
                "1 of 1".into(),
                keys[1].into(),
                keys[2].into(),
            ]
        );
        // Narrower windows shorten the words first, then drop them.
        assert!(
            seen.iter()
                .any(|parts| parts[0] == "agent edit \u{201c}The\u{2026}\u{201d}")
        );
        assert!(seen.iter().any(|parts| parts[0] == "agent edit"));
        // The count goes before the name does.
        assert!(
            seen.iter()
                .any(|parts| parts == &["agent edit", keys[1], keys[2]])
        );
        assert!(seen.iter().all(|parts| parts[0] != "1 of 1"));
        // Two edits: the move keys show, and go only after every word.
        let thanks = snap.text.find("Thanks").unwrap();
        snap.hunks.push(fell_core::compose::HunkView {
            id: 2,
            kind: super::HunkKind::Body,
            start: thanks,
            end: thanks + "Thanks".len(),
            label: String::new(),
            previous: String::new(),
        });
        let seen = sweep(&snap);
        assert_eq!(
            seen.last().unwrap()[1..],
            ["1 of 2", keys[0], keys[1], keys[2]]
        );
        for parts in &seen {
            if !parts.contains(&keys[0].to_string()) {
                assert!(!parts[0].starts_with("agent edit"), "{parts:?}");
            }
        }
    }

    #[test]
    fn the_status_line_does_not_quote_an_edit_the_bar_shows() {
        let line = "The [March plan](https://files.example/plan-v2) is attached.";
        let snap = one_body_edit(&format!("{line}\n"), line);
        assert_eq!(super::edit_name(&snap.hunks[0]), "the edit");
    }

    #[test]
    fn a_long_status_is_cut_at_a_word() {
        let status = "the agent changed the subject · u undo";
        assert_eq!(super::fit_words(status, 1000.0, &eight_px), status);
        // Never mid-word, never ending on a dot.
        let cut = super::fit_words(
            status,
            eight_px("the agent changed the subject ·…"),
            &eight_px,
        );
        assert_eq!(cut, "the agent changed the subject…");
        let cut = super::fit_words(status, eight_px("the agent chang"), &eight_px);
        assert_eq!(cut, "the agent…");
        assert!(eight_px(&cut) <= eight_px("the agent chang"));
        // A word already cut keeps its one ellipsis.
        let quoted = "kept \u{201c}The plan is\u{2026}\u{201d} · u undo";
        let cut = super::fit_words(
            quoted,
            eight_px("kept \u{201c}The plan is\u{2026}\u{201d} ·"),
            &eight_px,
        );
        assert_eq!(cut, "kept \u{201c}The plan is\u{2026}\u{201d}");
    }

    #[test]
    fn a_struck_line_ends_at_its_last_word() {
        let text =
            "The [March plan](https://files.example/plan) is attached, with the lodge booking";
        let lines = super::break_lines(text, 48.0 * 8.0, &eight_px);
        assert_eq!(lines.join(" "), text);
        for line in &lines {
            assert_eq!(line.trim(), line);
            assert!(eight_px(line) <= 48.0 * 8.0);
        }
        // A word wider than the line breaks inside itself.
        let lines = super::break_lines(
            "see https://files.example/a-very-long-path ok",
            10.0 * 8.0,
            &eight_px,
        );
        assert!(lines.iter().all(|line| eight_px(line) <= 80.0), "{lines:?}");
        assert_eq!(
            lines.concat().replace(' ', ""),
            "seehttps://files.example/a-very-long-pathok"
        );
    }
}
