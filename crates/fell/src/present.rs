//! Row and conversation presentation. Pure functions, no GPUI.

use fell_core::compose::DraftSummary;
use fell_core::{FolderRole, FolderRoles, FolderThread, Message};

use crate::text_render::{BodyBlock, RenderedBody, render_stored_body};

pub const NARROW_WIDTH: f32 = 880.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    List,
    Conversation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameSpan {
    pub text: String,
    pub emphasis: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadRow {
    pub id: String,
    pub spans: Vec<NameSpan>,
    pub extra: usize,
    pub message_count: usize,
    pub starred: bool,
    pub has_attachment: bool,
    pub date_unix: i64,
    pub subject: String,
    pub snippet: String,
    pub labels: Vec<String>,
    /// The first label is a warning (a draft that may have gone out), drawn
    /// in the error colour, not as a tag.
    pub warn: bool,
    pub unread: bool,
    /// The compose draft this row stands for, in the drafts list. `None`
    /// for a thread.
    pub draft: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConvMessage {
    /// The message's Message-ID, which open state is kept by.
    pub id: String,
    pub sender: String,
    pub email: String,
    /// The message's own `From:` and `Subject:`, which the HTML body is
    /// prepared with: a forward's subject keeps its history open, and a
    /// logo named for the sender draws no `Image blocked` line.
    pub mail_headers: Vec<(String, String)>,
    pub folders: Vec<String>,
    pub date_unix: i64,
    pub snippet: String,
    /// The stored text: what a message with no HTML part paints.
    pub body: RenderedBody,
    /// The message's HTML, once read from the store and prepared.
    pub rich: Rich,
    pub expanded: bool,
    /// Only parts with the stored Attachment role, in MIME order.
    pub attachments: Vec<fell_core::MimePart>,
    /// Who else got the message. `None` when nothing is stored.
    pub recipients: Option<RecipientLine>,
    /// A draft's "In reply to" or "Forwarding" line, as its compose window
    /// says it. `None` for mail and for a new message.
    pub context: Option<String>,
    /// Whether sync has downloaded the body. A first sync lists rows from
    /// their headers, then fills bodies in.
    pub body_state: fell_core::BodyState,
}

/// What the conversation says in place of a body it does not have.
pub fn missing_body_line(state: fell_core::BodyState) -> Option<String> {
    match state {
        fell_core::BodyState::Stored => None,
        fell_core::BodyState::Pending => Some("Not downloaded yet".into()),
        fell_core::BodyState::TooLarge => Some(format!(
            "Too large to download (over {} MB)",
            fell_core::MAX_MESSAGE_BYTES / (1024 * 1024)
        )),
    }
}

/// The open thread has a message whose body sync has not downloaded yet,
/// whichever it is: each sync write is read again for it.
pub fn awaiting_bodies(shown: &[ConvMessage]) -> bool {
    shown
        .iter()
        .any(|message| message.body_state == fell_core::BodyState::Pending)
}

pub fn same_messages(shown: &[ConvMessage], read: &[ConvMessage]) -> bool {
    shown.len() == read.len() && shown.iter().zip(read).all(|(old, new)| old.id == new.id)
}

/// Body changes for the same messages in the same order. An empty list
/// still allows a reread to update metadata without a body fade.
pub fn bodies_filled(shown: &[ConvMessage], read: &[ConvMessage]) -> Option<Vec<usize>> {
    if !same_messages(shown, read) {
        return None;
    }
    Some(
        shown
            .iter()
            .zip(read)
            .enumerate()
            .filter(|(_, (old, new))| {
                let replaced_html =
                    matches!(new.rich, Rich::Html(_) | Rich::PlainPart(_)) && old.rich != new.rich;
                old.body != new.body || old.body_state != new.body_state || replaced_html
            })
            .map(|(index, _)| index)
            .collect(),
    )
}

/// A conversation that fits before new bodies arrive can reveal the newest
/// message after layout. In a scrollable conversation every offset is a
/// possible reading position, including the top.
pub fn conversation_fits(max_offset: f32) -> bool {
    max_offset <= 1.0
}

/// Room a message's start needs on screen to count as in view.
const START_IN_VIEW_PX: f32 = 48.0;

/// The offset that shows the start of the message at `item_top` (scroll
/// content coordinates, as GPUI lays them out), or `None` when its start
/// is in view already and nothing should move.
pub fn offset_to_show_start(
    item_top: f32,
    view_top: f32,
    view_bottom: f32,
    offset: f32,
    max: f32,
) -> Option<f32> {
    let on_screen = item_top + offset;
    if on_screen >= view_top - 0.5 && on_screen <= view_bottom - START_IN_VIEW_PX {
        return None;
    }
    let target = (view_top - item_top).clamp(-max, 0.0);
    ((target - offset).abs() > 0.5).then_some(target)
}

/// What an expanded message paints for its body.
#[derive(Clone)]
pub enum Rich {
    /// No HTML part is stored: paint [`ConvMessage::body`].
    Plain,
    /// The stored plain part, used when the HTML has no native output.
    PlainPart(std::sync::Arc<RenderedBody>),
    /// No parts are stored at all yet (mail synced before they were kept).
    /// Paints the stored text; opening the message asks sync for the parts.
    Missing,
    /// An HTML part is stored and is being read and prepared off the UI
    /// thread. The body's space is reserved until it is ready.
    Pending,
    Html(std::sync::Arc<crate::mail_body::HtmlBody>),
}

impl PartialEq for Rich {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Html(a), Self::Html(b)) => std::sync::Arc::ptr_eq(a, b),
            (Self::PlainPart(a), Self::PlainPart(b)) => std::sync::Arc::ptr_eq(a, b),
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}

impl Eq for Rich {}

impl std::fmt::Debug for Rich {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Plain | Self::PlainPart(_) => "Plain",
            Self::Missing => "Missing",
            Self::Pending => "Pending",
            Self::Html(_) => "Html",
        })
    }
}

/// Who else got a message, for the lines under an expanded message's sender.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipientLine {
    /// One line of names: "to me · cc Ada, Maya, Sam".
    pub short: String,
    /// To and Cc names left off `short`; the line says "+N more".
    pub hidden: usize,
    /// "bcc Riya" on the person's own sent mail. Never folded into the
    /// count, so a Bcc always shows on the closed line.
    pub bcc: Option<String>,
    /// Every To and Cc name, by group, for fitting the closed line to the
    /// width on screen whole person by whole person.
    pub names: Vec<(&'static str, Vec<String>)>,
    /// How many names the closed line shows at most.
    pub budget: usize,
    /// Every recipient with an address, a row per group, once opened.
    pub groups: Vec<RecipientGroup>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipientGroup {
    /// "to", "cc", or "bcc".
    pub label: &'static str,
    pub people: Vec<String>,
    /// Bcc on the person's own mail: shown to them alone.
    pub only_you: bool,
}

/// Names on the short line. One more than this shows the last name instead
/// of "+1 more".
const SHOWN_RECIPIENTS: usize = 4;

/// To and Cc for anyone's message. Bcc only on the person's own sent mail:
/// Fell keeps it locally (a Sent copy has no Bcc header), and it never
/// describes someone else's message.
pub fn recipient_line(message: &Message, me: &str) -> Option<RecipientLine> {
    let from_me = message.headers.from.email.eq_ignore_ascii_case(me);
    let groups: [(&str, &'static str, &[fell_core::Address]); 3] = [
        ("to", "to", &message.to),
        ("cc", "cc", &message.cc),
        ("bcc", "bcc", if from_me { &message.bcc } else { &[] }),
    ];
    let total: usize = groups.iter().map(|(_, _, people)| people.len()).sum();
    if total == 0 {
        return None;
    }
    let visible: usize = groups[..2].iter().map(|(_, _, people)| people.len()).sum();
    let budget = if visible == SHOWN_RECIPIENTS + 1 {
        visible
    } else {
        SHOWN_RECIPIENTS
    };
    let mut short = Vec::new();
    let mut full = Vec::new();
    let mut bcc = None;
    let mut all_names: Vec<(&'static str, Vec<String>)> = Vec::new();
    let mut shown = 0;
    for (label, full_label, people) in groups {
        if people.is_empty() {
            continue;
        }
        let names: Vec<String> = people
            .iter()
            .map(|person| recipient_name(person, me))
            .collect();
        if label == "bcc" {
            bcc = Some(match names.len() {
                1 => format!("bcc {}", names[0]),
                more => format!("bcc {} +{}", names[0], more - 1),
            });
        } else {
            all_names.push((label_of(label), names.clone()));
            let room = budget.saturating_sub(shown).min(names.len());
            if room > 0 {
                short.push(format!("{label} {}", names[..room].join(", ")));
                shown += room;
            }
        }
        full.push(RecipientGroup {
            label: full_label,
            only_you: label == "bcc",
            people: people
                .iter()
                .zip(&names)
                .map(|(person, name)| {
                    if name.eq_ignore_ascii_case(&person.email) {
                        name.clone()
                    } else {
                        format!("{name} <{}>", person.email)
                    }
                })
                .collect(),
        });
    }
    Some(RecipientLine {
        short: short.join(" · "),
        hidden: visible - shown,
        bcc,
        names: all_names,
        budget,
        groups: full,
    })
}

fn label_of(label: &str) -> &'static str {
    if label == "to" { "to" } else { "cc" }
}

/// The closed line that fits: the most whole people (up to `line.budget`)
/// for which `fits(names, tail)` holds, the tail that goes with them
/// (" +N more · bcc X · q"), and how many are left out. Never drops a
/// name partway, so the count always matches what is on screen.
pub fn fit_recipients(
    line: &RecipientLine,
    fits: impl Fn(&str, &str) -> bool,
) -> (String, String, usize) {
    let total: usize = line.names.iter().map(|(_, names)| names.len()).sum();
    let tail = |hidden: usize| {
        let bcc = line
            .bcc
            .as_ref()
            .map(|bcc| format!(" · {bcc}"))
            .unwrap_or_default();
        // `q` opens the groups. Its key shows only when they hold more
        // than the line: names left off, or a Bcc.
        if hidden > 0 {
            format!(" +{hidden} more{bcc} · q")
        } else if bcc.is_empty() {
            String::new()
        } else {
            format!("{bcc} · q")
        }
    };
    let most = line.budget.min(total);
    for shown in (1..=most).rev() {
        let text = first_people(&line.names, shown);
        let rest = tail(total - shown);
        if fits(&text, &rest) {
            return (text, rest, total - shown);
        }
    }
    // Not even one person fits beside the tail: show the first anyway, and
    // let the caller shorten that one name.
    let hidden = total.saturating_sub(1);
    (
        first_people(&line.names, 1.min(total)),
        tail(hidden),
        hidden,
    )
}

/// "to Ada · cc Maya, Sam" for the first `count` people across groups.
fn first_people(groups: &[(&'static str, Vec<String>)], count: usize) -> String {
    let mut parts = Vec::new();
    let mut left = count;
    for (label, names) in groups {
        if left == 0 {
            break;
        }
        let take = left.min(names.len());
        if take > 0 {
            parts.push(format!("{label} {}", names[..take].join(", ")));
            left -= take;
        }
    }
    parts.join(" · ")
}

/// "me" only for the person's own address. Someone else whose name or
/// mailbox reads "me" is shown by address, so the two never look alike.
fn recipient_name(person: &fell_core::Address, me: &str) -> String {
    if person.email.eq_ignore_ascii_case(me) {
        return "me".into();
    }
    let name = display_name(&person.name, &person.email, me);
    if name.eq_ignore_ascii_case("me") {
        person.email.clone()
    } else {
        name
    }
}

pub fn visible_panes(width: f32, pane: Pane, full_width: bool) -> (bool, bool) {
    if width < NARROW_WIDTH {
        return (pane == Pane::List, pane == Pane::Conversation);
    }
    if full_width && pane == Pane::Conversation {
        return (false, true);
    }
    (true, true)
}

/// How the list follows the selection when a folder read lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Landing {
    /// Keep the selected row where it is on screen.
    Hold,
    /// A cursor move that waited for rows (`G`, `gg`, a page past the
    /// loaded rows) moved the selection: glide to it.
    Glide,
    /// Another folder, or a change the reader made: show the selection now.
    Jump,
}

pub fn landing(switched: bool, holds_viewport: bool, same_thread: bool) -> Landing {
    match (switched, holds_viewport, same_thread) {
        (false, true, true) => Landing::Hold,
        (false, true, false) => Landing::Glide,
        _ => Landing::Jump,
    }
}

/// How tall a palette or folder-picker card is drawn to hold `rows_px`
/// of items: its 1 px border top and bottom, the 44 px query line
/// and 4 px above and below the rows. Never taller than `max` or the
/// `room` the window leaves, never shorter than the query line.
pub fn card_height(rows_px: f32, max: f32, room: f32) -> f32 {
    let fit = 2.0 + 44.0 + 8.0 + rows_px;
    fit.min(max).min(room).max(2.0 + 44.0)
}

/// The list scroll offset (0 at the top, negative further down) that shows
/// row `index` of `rows` rows of height `row` in a viewport `view` tall,
/// starting from `offset`. A row already in full view stays put. `moved`:
/// a key moved the selection by that many rows. The view then moves by as
/// many rows, so the selection keeps its place on screen (`j` at the bottom
/// edge scrolls one row, `Ctrl-d` a page, `gg` and `G` to the ends).
/// Otherwise a row off screen goes to the top.
pub fn list_scroll_target(
    offset: f32,
    index: usize,
    moved: Option<i64>,
    rows: usize,
    row: f32,
    view: f32,
) -> f32 {
    let max = (rows as f32 * row - view).max(0.0);
    let top = index as f32 * row;
    let shows = |offset: f32| top >= -offset - 0.5 && top + row <= -offset + view + 0.5;
    if shows(offset) {
        return offset.clamp(-max, 0.0);
    }
    let target = match moved {
        Some(moved) => {
            let shifted = offset - moved as f32 * row;
            if shows(shifted) {
                shifted
            } else if top < -shifted {
                -top
            } else {
                view - top - row
            }
        }
        None => -top,
    };
    target.clamp(-max, 0.0)
}

pub fn thread_rows(threads: &[FolderThread], me: &str) -> Vec<ThreadRow> {
    threads
        .iter()
        .map(|thread| ThreadRow {
            id: thread.thread_id.0.clone(),
            spans: participant_spans(thread, me).0,
            extra: participant_spans(thread, me).1,
            message_count: thread.message_count,
            starred: thread.starred,
            has_attachment: thread.has_attachment,
            date_unix: thread.last_date_unix,
            subject: one_line(&thread.subject),
            snippet: one_line(&thread.snippet),
            labels: thread.labels.clone(),
            warn: false,
            unread: thread.unread,
            draft: None,
        })
        .collect()
}

/// The row id of compose draft `id`. Thread ids never start with this.
pub fn draft_row_id(id: &str) -> String {
    format!("draft:{id}")
}

/// What an original no longer in the mailbox reads as, after "In reply
/// to", in the drafts list and in the compose window.
pub const REPLY_GONE: &str = "a message not in this mailbox";

/// Rows for compose drafts, shown above the synced Drafts folder: who the
/// draft is to, `check Sent` when it may have gone out, and the agent it
/// belongs to. Inside Drafts every row is a draft, so none says so.
pub fn draft_rows(drafts: &[DraftSummary]) -> Vec<ThreadRow> {
    drafts
        .iter()
        .map(|draft| {
            let names: Vec<String> = draft
                .to
                .iter()
                .chain(&draft.cc)
                .map(|raw| recipient_label(raw, &draft.known_names))
                .collect();
            // "to Ada": the column names who a draft is to, not who sent
            // it, as it does for threads.
            let spans = to_spans(&names);
            let mut labels = Vec::new();
            if draft.maybe_sent {
                labels.push("check Sent".to_string());
            }
            let agent: String = draft
                .owner
                .trim_start_matches("agent:")
                .chars()
                .filter(|ch| !ch.is_control())
                .take(24)
                .collect();
            if !agent.trim().is_empty() {
                labels.push(agent);
            }
            ThreadRow {
                id: draft_row_id(&draft.id),
                spans,
                extra: names.len().saturating_sub(3),
                message_count: 1,
                starred: false,
                has_attachment: false,
                date_unix: draft.updated_unix,
                subject: if draft.subject.trim().is_empty() {
                    "(no subject)".into()
                } else {
                    one_line(&draft.subject)
                },
                snippet: markdown_snippet(&draft.body),
                labels,
                warn: draft.maybe_sent,
                unread: false,
                draft: Some(draft.id.clone()),
            }
        })
        .collect()
}

/// A draft in the reading pane: one message, from the person, as it reads.
pub fn draft_conversation(draft: &DraftSummary, me: &str, now: i64) -> Vec<ConvMessage> {
    let address = |raw: &String| {
        let (name, email) = fell_core::compose::mailbox_parts(raw)
            .unwrap_or_else(|| (None, raw.trim().to_string()));
        // The name the draft gives, or the one the mailbox knows from mail
        // the address sent, or else the address, as the row shows it:
        // never the bare local part.
        let name = name
            .filter(|name| !name.trim().is_empty())
            .or_else(|| draft.known_names.get(&email.to_lowercase()).cloned())
            .or_else(|| Some(email.clone()));
        fell_core::Address { name, email }
    };
    let from = address(&draft.from);
    let message_id = fell_core::MessageId::new(draft_row_id(&draft.id));
    let message = Message {
        account_id: fell_core::AccountId(String::new()),
        headers: fell_core::MessageHeaders {
            message_id: message_id.clone(),
            proton_conversation_id: None,
            in_reply_to: None,
            references: Vec::new(),
            from,
            subject: draft.subject.clone(),
            date_unix: draft.updated_unix,
        },
        remote_id: fell_core::RemoteId(String::new()),
        thread_id: fell_core::ThreadId::from_message_id(&message_id),
        snippet: markdown_snippet(&draft.body),
        body_text: Some(draft.body.clone()),
        unread: false,
        folders: Vec::new(),
        labels: Vec::new(),
        arrived_via: Vec::new(),
        to: draft.to.iter().map(address).collect(),
        cc: draft.cc.iter().map(address).collect(),
        bcc: draft.bcc.iter().map(address).collect(),
        starred: false,
        has_attachment: false,
        archived: false,
        snooze_until: None,
        list_id: None,
        reply_to: Some(Vec::new()),
        attachment_names: Vec::new(),
        parts: None,
    };
    // The folder line is replaced below, so no account roles are needed.
    let mut shown = conversation(&[message], me, &FolderRoles::default(), &[], "");
    for message in &mut shown {
        // Every row in Drafts is a draft, so the header doesn't say so. The
        // row's chip says what to do, in the error colour; here, only why,
        // so the warning is said once.
        message.folders = if draft.maybe_sent {
            vec!["may have been sent".into()]
        } else {
            Vec::new()
        };
        message.expanded = true;
        // The markdown the person wrote, styled as the compose window and
        // the recipient see it, not as source.
        if !draft.body.trim().is_empty() {
            message.body = RenderedBody {
                blocks: vec![BodyBlock::Markdown {
                    doc: std::sync::Arc::new(fell_core::compose::markdown_to_doc(&draft.body)),
                    agent: !draft.owner.trim().is_empty(),
                }],
            };
        }
        message.context = draft_context(draft, me, now);
    }
    shown
}

/// "In reply to Jon · Plans (2d)" or "Forwarding Ada · Q3 (Nov 2)": what a
/// draft answers, in the compose window's words.
/// A reply to the person's own message says "me", as the list does.
pub fn draft_context(draft: &DraftSummary, me: &str, now: i64) -> Option<String> {
    let line = |label: &str, who: &str, subject: &str, when: Option<i64>| {
        let who = display_safe(&one_line(who));
        let subject = display_safe(&one_line(subject));
        let mut line = format!("{label} {who}");
        if !subject.is_empty() {
            line.push_str(&format!(" · {subject}"));
        }
        if let Some(when) = when {
            line.push_str(&format!(" ({})", relative_date(now, when)));
        }
        line
    };
    if let Some(parent) = &draft.reply_context {
        let mine =
            !me.trim().is_empty() && parent.sender_email.trim().eq_ignore_ascii_case(me.trim());
        let who = if mine { "me" } else { parent.sender.as_str() };
        return Some(line(
            "In reply to",
            who,
            &parent.subject,
            Some(parent.date_unix),
        ));
    }
    // The original is gone from the mailbox: say so, not its raw id.
    if draft
        .in_reply_to
        .as_deref()
        .is_some_and(|id| !id.trim().is_empty())
    {
        return Some(line("In reply to", REPLY_GONE, "", None));
    }
    draft.forwarded.as_ref().map(|forward| {
        // The block's Date line is wall-clock time where it was written.
        let when = forward.date_wall.map(|wall| wall - local_offset(wall));
        line("Forwarding", &forward.sender, &forward.subject, when)
    })
}

/// A compose draft's body on one line, as it reads: markdown markers,
/// link destinations and image references left out.
pub fn markdown_snippet(body: &str) -> String {
    use fell_core::compose::{BlockBody, Inline};
    fn inlines(out: &mut Vec<String>, items: &[Inline]) {
        for item in items {
            match item {
                Inline::Text(text) | Inline::Code(text) => out.push(text.clone()),
                Inline::Em(inner) | Inline::Strong(inner) => inlines(out, inner),
                Inline::Link { text, .. } => out.push(text.clone()),
                Inline::Image { .. } => {}
            }
        }
    }
    fn blocks(out: &mut Vec<String>, items: &[fell_core::compose::Block]) {
        for block in items {
            match &block.body {
                BlockBody::Heading { inlines: items, .. }
                | BlockBody::Paragraph { inlines: items } => {
                    inlines(out, items);
                    out.push(" ".into());
                }
                BlockBody::List { items, .. } => {
                    for item in items {
                        inlines(out, item);
                        out.push(" ".into());
                    }
                }
                BlockBody::Quote { blocks: inner } => blocks(out, inner),
                BlockBody::Code { text } => {
                    out.push(text.clone());
                    out.push(" ".into());
                }
                BlockBody::Image { .. } => {}
            }
        }
    }
    let doc = fell_core::compose::markdown_to_doc(body);
    let mut out = Vec::new();
    blocks(&mut out, &doc.blocks);
    one_line(&out.concat())
}

/// Synced drafts in the Drafts folder name who they are to, as compose
/// drafts do, instead of their sender (always "me").
pub fn address_draft_rows(
    rows: &mut [ThreadRow],
    recipients: &std::collections::HashMap<fell_core::ThreadId, Vec<fell_core::Address>>,
    me: &str,
) {
    for row in rows.iter_mut() {
        // A draft is not mail waiting to be read.
        row.unread = false;
        let Some(people) = recipients.get(&fell_core::ThreadId(row.id.clone())) else {
            continue;
        };
        let names: Vec<String> = people
            .iter()
            .map(|person| match person.name.as_deref().map(str::trim) {
                Some(name) if !name.is_empty() => one_line(name),
                _ if person.email.eq_ignore_ascii_case(me) => "me".into(),
                _ => one_line(&person.email),
            })
            .collect();
        row.extra = names.len().saturating_sub(3);
        row.spans = to_spans(&names);
    }
}

/// "to Ada", then the next names, or "No recipients".
fn to_spans(names: &[String]) -> Vec<NameSpan> {
    if names.is_empty() {
        return vec![NameSpan {
            text: "No recipients".into(),
            emphasis: false,
        }];
    }
    names
        .iter()
        .take(3)
        .enumerate()
        .map(|(index, name)| NameSpan {
            text: if index == 0 {
                format!("to {name}")
            } else {
                name.clone()
            },
            emphasis: false,
        })
        .collect()
}

/// Text from outside (an agent name, a sender, a subject) without line
/// breaks or the invisible marks that reorder or hide what is drawn.
pub fn display_safe(text: &str) -> String {
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

/// A recipient as a draft row names them: the display name the draft
/// gives, or else the address as typed. A name is never made up from the
/// address: "fell-qa-test" read as "Fell-qa-test" looks chosen, and hides
/// which address it is.
///
/// The name the mailbox knows for the address from mail it sent (`known`,
/// by lowercase address) is a real name, so it shows.
fn recipient_label(raw: &str, known: &std::collections::BTreeMap<String, String>) -> String {
    match fell_core::compose::mailbox_parts(raw) {
        Some((Some(name), _)) if !name.trim().is_empty() => display_safe(&one_line(&name)),
        Some((_, email)) => match known.get(&email.to_lowercase()) {
            Some(name) if !name.trim().is_empty() => display_safe(&one_line(name)),
            _ => one_line(&email),
        },
        None => one_line(raw),
    }
}

/// `roles` names each message's folders the way the account uses them;
/// `names` are the account's folders, so a label that two folders share
/// carries the folder's own name ([`FolderRoles::label_among`]).
/// `viewing` is the folder the list shows: inside Drafts, a draft's
/// header doesn't say "Draft", since every row there is one.
pub fn conversation(
    messages: &[Message],
    me: &str,
    roles: &FolderRoles,
    names: &[String],
    viewing: &str,
) -> Vec<ConvMessage> {
    let in_drafts = roles.is(FolderRole::Drafts, viewing);
    let last = messages.len().saturating_sub(1);
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let email = message.headers.from.email.clone();
            ConvMessage {
                id: message.headers.message_id.as_str().to_string(),
                mail_headers: vec![
                    ("Subject".into(), message.headers.subject.clone()),
                    ("From".into(), from_header(&message.headers.from)),
                ],
                sender: display_name(&message.headers.from.name, &email, me),
                email,
                folders: message
                    .folders
                    .iter()
                    // A message in Drafts is a draft: "Draft", unless the
                    // reader is in Drafts, where every row is one.
                    .filter_map(|folder| {
                        if !roles.is(FolderRole::Drafts, folder) {
                            Some(roles.label_among(folder, names))
                        } else if in_drafts {
                            None
                        } else {
                            Some("Draft".to_string())
                        }
                    })
                    .chain(message.labels.iter().cloned())
                    .collect(),
                date_unix: message.headers.date_unix,
                snippet: one_line(&message.snippet),
                body: render_stored_body(
                    message.body_text.as_deref(),
                    &message.snippet,
                    &message.headers.subject,
                ),
                rich: Rich::Plain,
                expanded: message.unread || index == last,
                attachments: Vec::new(),
                recipients: recipient_line(message, me),
                context: None,
                body_state: fell_core::BodyState::Stored,
            }
        })
        .collect()
}

fn participant_spans(thread: &FolderThread, me: &str) -> (Vec<NameSpan>, usize) {
    let mut ordered: Vec<(String, String)> = Vec::new();
    let latest = display_name(&thread.latest_name, &thread.latest_email, me);
    ordered.push((latest, thread.latest_email.to_ascii_lowercase()));
    for person in thread.participants.iter().rev() {
        let key = person.email.to_ascii_lowercase();
        if ordered.iter().any(|(_, email)| email == &key) {
            continue;
        }
        ordered.push((display_name(&person.name, &person.email, me), key));
    }
    let extra = ordered.len().saturating_sub(3);
    let spans = ordered
        .into_iter()
        .take(3)
        .enumerate()
        .map(|(index, (text, _))| NameSpan {
            text,
            emphasis: index == 0,
        })
        .collect();
    (spans, extra)
}

pub fn display_name(name: &Option<String>, email: &str, me: &str) -> String {
    if email.eq_ignore_ascii_case(me) {
        return "me".into();
    }
    if let Some(name) = name {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    email.split('@').next().unwrap_or(email).to_string()
}

/// Folders for the `gf` picker: the account's role folders first, then the
/// rest of `stored`. A stored name that is another spelling of a role
/// folder's name (`junk` for `Spam`) shares that row.
pub fn merge_folders(roles: &FolderRoles, stored: &[String]) -> Vec<String> {
    let mut out = roles.picker(&[]);
    for name in stored {
        let alias_of_shown = fell_core::folder_aliases(name).iter().any(|alias| {
            out.iter()
                .any(|have| fell_core::folder_aliases(have).contains(alias))
        });
        if alias_of_shown || out.iter().any(|have| have.eq_ignore_ascii_case(name)) {
            continue;
        }
        out.push(name.clone());
    }
    out
}

/// Message whose box contains the top of the viewport.
///
/// `spans` are `(top, bottom)` in the same space as `viewport_top`. A page
/// that stays inside one box keeps that message; the next message starts
/// when the viewport top reaches its box.
pub fn message_at_top(spans: &[(f32, f32)], viewport_top: f32) -> Option<usize> {
    if spans.is_empty() {
        return None;
    }
    Some(
        spans
            .iter()
            .position(|(_, bottom)| viewport_top < *bottom)
            .unwrap_or(spans.len() - 1),
    )
}

/// Message under the viewport after a page, using one frame's measurements.
///
/// `item_bounds` are window positions from the layout that ran while
/// `measured_offset` was the scroll offset. GPUI records those positions
/// before it applies the offset, so they are not shifted by it. The page
/// distance is `measured_offset - new_offset`, and that is added to the
/// content position of the measured frame (`-measured_offset`). The new
/// offset is never subtracted from the previous frame's boxes.
pub fn message_after_page(
    item_bounds: &[(f32, f32)],
    scrollport_top: f32,
    measured_offset: f32,
    new_offset: f32,
) -> Option<usize> {
    let content_top = -measured_offset + (measured_offset - new_offset);
    let spans: Vec<(f32, f32)> = item_bounds
        .iter()
        .map(|(top, bottom)| (top - scrollport_top, bottom - scrollport_top))
        .collect();
    message_at_top(&spans, content_top)
}

/// Cursor after a conversation scroll.
///
/// When the clamped offset does not change, the cursor stays. A downward
/// scroll does not move the highlight onto an earlier message.
pub fn cursor_after_nudge(
    current: usize,
    top: Option<usize>,
    downward: bool,
    moved: bool,
) -> usize {
    if !moved {
        return current;
    }
    match top {
        Some(top) if !(downward && top < current) => top,
        _ => current,
    }
}

pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn relative_date(now: i64, then: i64) -> String {
    relative_date_label(now, then, false)
}

pub(crate) fn compact_relative_date(now: i64, then: i64) -> String {
    relative_date_label(now, then, true)
}

fn relative_date_label(now: i64, then: i64, compact: bool) -> String {
    // A future timestamp is not "now". Compare first: saturating_sub never goes negative.
    if then > now {
        return month_day(now, then, compact);
    }
    let delta = now.saturating_sub(then);
    if delta < 45 {
        return "now".into();
    }
    if delta < 60 * 60 {
        return format!("{}m", (delta / 60).max(1));
    }
    let (ny, nmo, nd, _, _, _) = local_parts(now);
    let (ty, tmo, td, th, tm, tw) = local_parts(then);
    if (ny, nmo, nd) == (ty, tmo, td) {
        if delta < 12 * 60 * 60 {
            return format!("{}h", (delta / 3600).max(1));
        }
        return format!("{th:02}:{tm:02}");
    }
    if yesterday(ny, nmo, nd) == (ty, tmo, td) {
        return if compact { "Yest." } else { "Yesterday" }.into();
    }
    if delta < 7 * 24 * 60 * 60 {
        if ty != ny {
            return month_day(now, then, compact);
        }
        return weekday(tw).into();
    }
    month_day(now, then, compact)
}

fn month_day(now: i64, then: i64, compact: bool) -> String {
    let (now_year, _, _, _, _, _) = local_parts(now);
    let (year, month, day, _, _, _) = local_parts(then);
    if year == now_year {
        format!("{} {day}", month_name(month))
    } else if compact {
        format!("{month}/{day}/{:02}", year.rem_euclid(100))
    } else {
        format!("{} {day}, {year}", month_name(month))
    }
}

fn yesterday(year: i32, month: u8, day: u8) -> (i32, u8, u8) {
    if day > 1 {
        return (year, month, day - 1);
    }
    if month > 1 {
        return (year, month - 1, days_in_month(year, month - 1));
    }
    (year - 1, 12, 31)
}

fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
    }
}

fn weekday(wday: u8) -> &'static str {
    ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
        .get(wday as usize)
        .copied()
        .unwrap_or("Day")
}

fn month_name(month: u8) -> &'static str {
    [
        "", "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .get(month as usize)
    .copied()
    .unwrap_or("Month")
}

/// Seconds east of UTC of the local zone at `unix`, or 0 when unknown.
pub fn local_offset(unix: i64) -> i64 {
    // SAFETY: `localtime_r` is thread-safe and writes only through `out`.
    unsafe {
        let seconds = unix as libc::time_t;
        let mut out = std::mem::zeroed::<libc::tm>();
        if libc::localtime_r(&seconds, &mut out).is_null() {
            return 0;
        }
        (out.tm_gmtoff as i64).clamp(-14 * 3600, 14 * 3600)
    }
}

fn local_parts(unix: i64) -> (i32, u8, u8, u8, u8, u8) {
    unsafe { local_parts_libc(unix) }.unwrap_or_else(|| utc_parts(unix))
}

/// `localtime_r` is documented to be thread-safe and writes only through `out`.
unsafe fn local_parts_libc(unix: i64) -> Option<(i32, u8, u8, u8, u8, u8)> {
    unsafe {
        let seconds = unix as libc::time_t;
        let mut out = std::mem::zeroed::<libc::tm>();
        if libc::localtime_r(&seconds, &mut out).is_null() {
            return None;
        }
        Some((
            out.tm_year + 1900,
            u8::try_from(out.tm_mon).ok()? + 1,
            u8::try_from(out.tm_mday).ok()?,
            u8::try_from(out.tm_hour).ok()?,
            u8::try_from(out.tm_min).ok()?,
            u8::try_from(out.tm_wday).ok()?,
        ))
    }
}

fn utc_parts(unix: i64) -> (i32, u8, u8, u8, u8, u8) {
    let day_seconds = 86_400_i64;
    let days = unix.div_euclid(day_seconds);
    let tod = unix.rem_euclid(day_seconds) as u32;
    let hour = (tod / 3600) as u8;
    let minute = ((tod % 3600) / 60) as u8;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u8;
    let year = (y + i64::from(month <= 2)) as i32;
    let wday = ((days + 4).rem_euclid(7)) as u8;
    (year, month, day, hour, minute, wday)
}

/// Fit a notice to `max_px`, as `measure` sizes text. The quoted name
/// gives up whole words first. If even one word does not fit, the clauses
/// between the outcome and the closing key go next, last first, and the
/// name is fitted again; so the outcome and the key always stay whole and
/// no clause is cut in the middle. Only an outcome too long alone is cut.
pub fn fit_notice(text: &str, max_px: f32, measure: impl Fn(&str) -> f32) -> String {
    if measure(text) <= max_px {
        return text.to_string();
    }
    if let Some(fitted) = shorten_name(text, max_px, &measure) {
        return fitted;
    }
    let mut parts: Vec<&str> = text.split(NOTICE_DOT).collect();
    while let Some(drop) = (1..parts.len().saturating_sub(1))
        .rev()
        .find(|index| !parts[*index].contains('\u{201c}'))
    {
        parts.remove(drop);
        let candidate = parts.join(NOTICE_DOT);
        if measure(&candidate) <= max_px {
            return candidate;
        }
        if let Some(fitted) = shorten_name(&candidate, max_px, &measure) {
            return fitted;
        }
    }
    // Not one word of the name fits: a stub of it says nothing, so the
    // name goes, and the outcome and key stay whole.
    if let Some(at) = parts.iter().position(|part| part.contains('\u{201c}'))
        && parts.len() > 1
    {
        let mut without = parts.clone();
        without.remove(at);
        let candidate = without.join(NOTICE_DOT);
        if measure(&candidate) <= max_px {
            return candidate;
        }
    }
    let text = parts.join(NOTICE_DOT);
    match quoted_span(&text) {
        Some((open, close)) => {
            let before = &text[..open + '\u{201c}'.len_utf8()];
            let after = &text[close..];
            let name = text[before.len()..close].trim_end_matches('…');
            cut_to(name, before, after, max_px, &measure)
        }
        None => cut_to(&text, "", "", max_px, &measure),
    }
}

/// How the inbox separates a notice's clauses.
pub(crate) const NOTICE_DOT: &str = "  ·  ";

fn quoted_span(text: &str) -> Option<(usize, usize)> {
    text.find('\u{201c}').and_then(|open| {
        text[open..]
            .find('\u{201d}')
            .map(|close| (open, open + close))
    })
}

/// `text` with its quoted name cut to whole words that fit, keeping at
/// least the first; `None` when no quoted name, or not even one word, fits.
fn shorten_name(text: &str, max_px: f32, measure: &impl Fn(&str) -> f32) -> Option<String> {
    let (open, close) = quoted_span(text)?;
    let before = &text[..open + '\u{201c}'.len_utf8()];
    let after = &text[close..];
    let name = text[before.len()..close].trim_end_matches('…');
    let words: Vec<&str> = name.split_whitespace().collect();
    (1..words.len())
        .rev()
        .map(|count| format!("{before}{}…{after}", words[..count].join(" ")))
        .find(|candidate| measure(candidate) <= max_px)
}

/// `head`, cut short: an ellipsis after its last word, with no space or
/// punctuation left in between ("Friday…", not "Friday.…").
pub fn ellipsis(head: &str) -> String {
    let word = head.trim_end_matches(|ch: char| {
        ch.is_whitespace()
            || matches!(
                ch,
                '.' | ','
                    | ';'
                    | ':'
                    | '!'
                    | '?'
                    | '-'
                    | '\u{2013}'
                    | '\u{2014}'
                    | '\u{b7}'
                    | '('
                    | '['
                    | '{'
                    | '\u{201c}'
                    | '\u{2018}'
            )
    });
    format!("{word}\u{2026}")
}

/// The longest start of `middle` that fits between `before` and `after`,
/// with an ellipsis.
fn cut_to(
    middle: &str,
    before: &str,
    after: &str,
    max_px: f32,
    measure: &impl Fn(&str) -> f32,
) -> String {
    let chars: Vec<char> = middle.chars().collect();
    let mut best = format!("{before}…{after}");
    let (mut lo, mut hi) = (1, chars.len());
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let short: String = chars[..mid].iter().collect();
        let candidate = format!("{before}{}{after}", ellipsis(&short));
        if measure(&candidate) <= max_px {
            best = candidate;
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    best
}

/// `Name <address>`, or the address alone, as a `From:` header writes it.
fn from_header(from: &fell_core::Address) -> String {
    match from.name.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => format!("{name} <{}>", from.email),
        _ => from.email.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fell_core::message::Address;
    use fell_core::{AccountId, MessageHeaders, MessageId, RemoteId, ThreadId};

    fn shown_message(id: &str, state: fell_core::BodyState, body: &str) -> ConvMessage {
        ConvMessage {
            id: id.into(),
            sender: "Ada".into(),
            email: "ada@x".into(),
            mail_headers: Vec::new(),
            folders: vec!["Inbox".into()],
            date_unix: 0,
            snippet: body.into(),
            body: render_stored_body(Some(body).filter(|text| !text.is_empty()), body, ""),
            rich: Rich::Plain,
            expanded: true,
            attachments: Vec::new(),
            recipients: None,
            context: None,
            body_state: state,
        }
    }

    #[test]
    fn a_reread_that_only_fills_bodies_is_told_apart_from_new_mail() {
        use fell_core::BodyState::{Pending, Stored};
        let shown = vec![
            shown_message("a", Pending, ""),
            shown_message("b", Pending, ""),
        ];
        // Any message, not only the latest.
        let older = vec![
            shown_message("a", Stored, "first"),
            shown_message("b", Pending, ""),
        ];
        assert_eq!(bodies_filled(&shown, &older), Some(vec![0]));
        assert!(awaiting_bodies(&shown));
        assert!(awaiting_bodies(&older), "the latest still waits");
        let latest_only = vec![
            shown_message("a", Pending, ""),
            shown_message("b", Stored, "second"),
        ];
        assert!(awaiting_bodies(&latest_only), "an older one still waits");
        let both = vec![
            shown_message("a", Stored, "first"),
            shown_message("b", Stored, "second"),
        ];
        assert_eq!(bodies_filled(&shown, &both), Some(vec![0, 1]));
        assert!(!awaiting_bodies(&both));
        let too_large = vec![shown_message("a", fell_core::BodyState::TooLarge, "")];
        assert!(!awaiting_bodies(&too_large), "nothing more will come");
        assert_eq!(bodies_filled(&shown, &shown), Some(vec![]));
        // A message that came or went is not a fill.
        let more = vec![
            shown_message("a", Pending, ""),
            shown_message("b", Pending, ""),
            shown_message("c", Stored, "new"),
        ];
        assert_eq!(bodies_filled(&shown, &more), None);
    }

    #[test]
    fn a_body_arriving_moves_the_pane_only_to_show_the_newest_start() {
        assert!(conversation_fits(0.0));
        assert!(!conversation_fits(400.0), "keep any reading position");
        // Viewport 100..700. The newest message starts in view: stay.
        assert_eq!(offset_to_show_start(300.0, 100.0, 700.0, 0.0, 900.0), None);
        // Older bodies pushed its start below the fold: glide to it.
        assert_eq!(
            offset_to_show_start(1_000.0, 100.0, 700.0, 0.0, 900.0),
            Some(-900.0)
        );
        // Never past the end.
        assert_eq!(
            offset_to_show_start(1_000.0, 100.0, 700.0, 0.0, 600.0),
            Some(-600.0)
        );
        // Its start only just peeks in at the bottom: still glide.
        assert_eq!(
            offset_to_show_start(680.0, 100.0, 700.0, 0.0, 900.0),
            Some(-580.0)
        );
    }

    #[test]
    fn a_body_not_downloaded_says_why() {
        use fell_core::BodyState::{Pending, Stored, TooLarge};
        assert_eq!(missing_body_line(Stored), None);
        assert_eq!(
            missing_body_line(Pending).as_deref(),
            Some("Not downloaded yet")
        );
        assert_eq!(
            missing_body_line(TooLarge).as_deref(),
            Some("Too large to download (over 32 MB)")
        );
    }

    #[test]
    fn a_card_is_as_tall_as_its_items_up_to_its_max() {
        // Five folders: no empty surface below the last (#171).
        assert_eq!(card_height(170.0, 380.0, 900.0), 54.0 + 170.0);
        assert_eq!(card_height(0.0, 420.0, 900.0), 54.0);
        assert_eq!(card_height(1440.0, 420.0, 900.0), 420.0);
        assert_eq!(card_height(1440.0, 420.0, 300.0), 300.0, "a short window");
        assert_eq!(
            card_height(1440.0, 420.0, 10.0),
            46.0,
            "the query line stays"
        );
    }

    #[test]
    fn the_list_scrolls_only_as_far_as_the_selection_needs() {
        // 100 rows of 68 px in a 680 px viewport: ten rows show.
        let target =
            |offset, index, moved| list_scroll_target(offset, index, moved, 100, 68.0, 680.0);
        assert_eq!(target(0.0, 5, Some(1)), 0.0, "in view: stays");
        assert_eq!(
            target(0.0, 10, Some(1)),
            -68.0,
            "j at the bottom edge: one row"
        );
        assert_eq!(
            target(-68.0, 0, Some(-1)),
            0.0,
            "k at the top edge: one row"
        );
        assert_eq!(
            target(0.0, 13, Some(10)),
            -680.0,
            "ctrl-d keeps the row's place"
        );
        assert_eq!(target(-3400.0, 0, Some(-55)), 0.0, "gg");
        assert_eq!(target(0.0, 99, Some(99)), -6120.0, "G stops at the end");
        assert_eq!(
            target(0.0, 40, None),
            -2720.0,
            "off screen, no key: to the top"
        );
        assert_eq!(target(0.0, 98, None), -6120.0, "never past the end");
        assert_eq!(
            list_scroll_target(0.0, 2, None, 3, 68.0, 680.0),
            0.0,
            "short list"
        );
    }

    #[test]
    fn a_draft_row_names_who_it_is_to() {
        let none = std::collections::BTreeMap::new();
        assert_eq!(
            recipient_label("Ada Lovelace <ada@example.com>", &none),
            "Ada Lovelace"
        );
        // #160: no name made up from the address.
        assert_eq!(recipient_label("ada@example.com", &none), "ada@example.com");
        assert_eq!(
            recipient_label("fell-qa-test@example.com", &none),
            "fell-qa-test@example.com"
        );
        assert_eq!(
            recipient_label("<j.smith@example.com>", &none),
            "j.smith@example.com"
        );
    }

    #[test]
    fn a_draft_names_a_recipient_the_mailbox_knows() {
        // A name from mail the address sent is real, so it shows; the
        // address the draft has typed is still what it is sent to.
        let known: std::collections::BTreeMap<String, String> = [(
            "ada@example.com".to_string(),
            "Ada\u{202e} Lovelace".to_string(),
        )]
        .into();
        assert_eq!(recipient_label("Ada@Example.com", &known), "Ada Lovelace");
        assert_eq!(
            recipient_label("Countess <ada@example.com>", &known),
            "Countess",
            "a name the draft gives wins"
        );
        let mut summary = draft("hi\n");
        summary.to = vec!["ada@example.com".into()];
        summary.cc = Vec::new();
        summary.known_names = [("ada@example.com".to_string(), "Ada Lovelace".to_string())].into();
        let row = &draft_rows(std::slice::from_ref(&summary))[0];
        assert_eq!(row.spans[0].text, "to Ada Lovelace");
        let shown = draft_conversation(&summary, "me@example.com", 2_000);
        let line = shown[0].recipients.as_ref().unwrap();
        assert!(line.short.contains("Ada Lovelace"), "{line:?}");
        // Inside Drafts, every row is a draft: the header doesn't say so.
        assert!(shown[0].folders.is_empty());
    }

    fn draft(body: &str) -> DraftSummary {
        DraftSummary {
            id: "d1".into(),
            from: "me@example.com".into(),
            to: vec!["fell-qa-test@example.com".into()],
            cc: vec!["Ada Lovelace <ada@example.com>".into()],
            bcc: Vec::new(),
            subject: "Plans".into(),
            body: body.into(),
            owner: String::new(),
            updated_unix: 1_000,
            maybe_sent: false,
            reply_context: None,
            forwarded: None,
            in_reply_to: None,
            known_names: Default::default(),
        }
    }

    const STYLED: &str = "Some **bold** text, a [link](https://example.com) and a list:\n\n- one\n";

    #[test]
    fn a_draft_snippet_is_the_text_without_markdown() {
        // #159: the row showed the raw markdown.
        assert_eq!(
            markdown_snippet(STYLED),
            "Some bold text, a link and a list: one"
        );
        assert_eq!(
            markdown_snippet("# Dates\n\n> quoted *note*\n"),
            "Dates quoted note"
        );
        let row = &draft_rows(&[draft(STYLED)])[0];
        assert_eq!(row.snippet, "Some bold text, a link and a list: one");
    }

    #[test]
    fn a_draft_preview_paints_its_markdown_styled() {
        let shown = draft_conversation(&draft(STYLED), "me@example.com", 2_000);
        let blocks = &shown[0].body.blocks;
        let [BodyBlock::Markdown { doc, .. }] = blocks.as_slice() else {
            panic!("the draft paints as markdown, not as source text: {blocks:?}");
        };
        assert_eq!(*doc.as_ref(), fell_core::compose::markdown_to_doc(STYLED));
        assert_eq!(shown[0].snippet, "Some bold text, a link and a list: one");
        // An empty draft keeps the plain empty body.
        let empty = draft_conversation(&draft(""), "me@example.com", 2_000);
        assert!(matches!(
            empty[0].body.blocks.as_slice(),
            [BodyBlock::Text(_)]
        ));
    }

    #[test]
    fn a_draft_row_names_only_what_sets_it_apart() {
        // Round 3: every row in Drafts is a draft, so a `draft` chip was
        // noise; the agent it belongs to still shows.
        let mine = draft("hi\n");
        assert!(draft_rows(std::slice::from_ref(&mine))[0].labels.is_empty());
        let mut agents = mine.clone();
        agents.owner = "agent:bot".into();
        assert_eq!(draft_rows(&[agents])[0].labels, vec!["bot"]);
        // A draft that may have gone out says "check Sent" once, in the
        // error colour on its row; the preview says only why.
        let mut maybe = mine;
        maybe.maybe_sent = true;
        let row = &draft_rows(&[maybe.clone()])[0];
        assert_eq!(row.labels, vec!["check Sent"]);
        assert!(row.warn);
        let shown = draft_conversation(&maybe, "me@example.com", 2_000);
        assert_eq!(shown[0].folders, ["may have been sent"]);
    }

    #[test]
    fn a_draft_row_and_preview_show_an_unnamed_address_as_typed() {
        // #160: "fell-qa-test@example.com" read as "Fell-qa-test".
        let row = &draft_rows(&[draft("hi\n")])[0];
        let names: Vec<&str> = row.spans.iter().map(|span| span.text.as_str()).collect();
        assert_eq!(names, vec!["to fell-qa-test@example.com", "Ada Lovelace"]);
        let shown = draft_conversation(&draft("hi\n"), "me@example.com", 2_000);
        let line = shown[0].recipients.as_ref().unwrap();
        assert_eq!(line.short, "to fell-qa-test@example.com · cc Ada Lovelace");
        assert_eq!(line.groups[0].people, vec!["fell-qa-test@example.com"]);
        assert_eq!(
            line.groups[1].people,
            vec!["Ada Lovelace <ada@example.com>"]
        );
        assert_eq!(shown[0].sender, "me");
    }

    #[test]
    fn a_draft_preview_says_what_it_answers() {
        // #136: a reply or forward in the drafts list says what it answers.
        let mut reply = draft("hi\n");
        reply.reply_context = Some(fell_core::compose::ReplyContext {
            sender: "Jon\u{202e} Smith".into(),
            sender_email: "jon@example.com".into(),
            reply_to: Some(Vec::new()),
            subject: "Q3\nplan".into(),
            date_unix: 2_000 - 3 * 86_400,
        });
        let shown = draft_conversation(&reply, "me@example.com", 2_000);
        let line = shown[0].context.as_deref().unwrap();
        // Untrusted header text cannot reorder the line or break it.
        assert!(
            line.starts_with("In reply to Jon Smith · Q3 plan ("),
            "{line}"
        );
        let mut forward = draft("fyi\n");
        forward.forwarded = Some(fell_core::compose::ForwardSummary {
            sender: "Ada Lovelace".into(),
            subject: "Budget".into(),
            date_wall: None,
            not_forwarded: None,
        });
        assert_eq!(
            draft_context(&forward, "me@example.com", 2_000).as_deref(),
            Some("Forwarding Ada Lovelace · Budget")
        );
        assert_eq!(
            draft_context(&draft("new\n"), "me@example.com", 2_000),
            None
        );
    }

    #[test]
    fn a_reply_to_my_own_mail_or_to_mail_that_is_gone_says_so() {
        let mut mine = draft("more\n");
        mine.reply_context = Some(fell_core::compose::ReplyContext {
            sender: "Caleb".into(),
            sender_email: "Me@Example.com".into(),
            reply_to: Some(Vec::new()),
            subject: "Plans".into(),
            date_unix: 2_000,
        });
        let line = draft_context(&mine, "me@example.com", 2_000).unwrap();
        assert!(line.starts_with("In reply to me · Plans"), "{line}");
        // The original is not in the mailbox: said in words, not its id.
        let mut gone = draft("hi\n");
        gone.in_reply_to = Some("<lost\u{202e}@example.com>".into());
        assert_eq!(
            draft_context(&gone, "me@example.com", 2_000).as_deref(),
            Some("In reply to a message not in this mailbox")
        );
    }

    #[test]
    fn an_agent_draft_preview_shows_where_links_go() {
        let mut summary = draft("Please [pay](https://evil.example/login)\n");
        let agent = |summary: &DraftSummary| {
            let shown = draft_conversation(summary, "me@example.com", 2_000);
            match shown[0].body.blocks.as_slice() {
                [BodyBlock::Markdown { agent, .. }] => *agent,
                other => panic!("{other:?}"),
            }
        };
        assert!(!agent(&summary));
        summary.owner = "agent:bot".into();
        assert!(agent(&summary), "the editor's rule for an agent's draft");
    }

    #[test]
    fn a_synced_draft_row_names_who_it_is_to() {
        // #99: synced drafts showed their sender, "me".
        let mut rows = vec![ThreadRow {
            id: "t1".into(),
            spans: vec![NameSpan {
                text: "me".into(),
                emphasis: true,
            }],
            extra: 0,
            message_count: 2,
            starred: false,
            has_attachment: false,
            date_unix: 0,
            subject: "Re: Plans".into(),
            snippet: String::new(),
            labels: Vec::new(),
            warn: false,
            unread: true,
            draft: None,
        }];
        let mut untouched = rows[0].clone();
        untouched.id = "t2".into();
        rows.push(untouched);
        let mut recipients = std::collections::HashMap::new();
        recipients.insert(
            ThreadId("t1".into()),
            vec![
                Address {
                    name: Some("Ada Lovelace".into()),
                    email: "ada@example.com".into(),
                },
                Address {
                    name: None,
                    email: "sam@example.com".into(),
                },
            ],
        );
        address_draft_rows(&mut rows, &recipients, "me@example.com");
        let names: Vec<&str> = rows[0]
            .spans
            .iter()
            .map(|span| span.text.as_str())
            .collect();
        assert_eq!(names, vec!["to Ada Lovelace", "sam@example.com"]);
        assert!(!rows[0].unread && !rows[1].unread, "no draft is unread");
        assert_eq!(
            rows[1].spans[0].text, "me",
            "a row with nothing known keeps its names"
        );
    }

    #[test]
    fn a_narrow_warning_drops_its_middle_clause_before_its_key() {
        let notice = "May have been sent  ·  check Sent before sending again  ·  \u{201c}Budget numbers\u{201d}  ·  gs sent";
        let width = |text: &str| text.chars().count() as f32;
        let fitted = fit_notice(notice, 60.0, width);
        assert_eq!(
            fitted,
            "May have been sent  ·  \u{201c}Budget numbers\u{201d}  ·  gs sent"
        );
        let fitted = fit_notice(notice, 48.0, width);
        assert_eq!(
            fitted,
            "May have been sent  ·  \u{201c}Budget…\u{201d}  ·  gs sent"
        );
        assert!(width(&fitted) <= 48.0);
        // Not one word of the subject fits: it goes, not a stub of it.
        assert_eq!(
            fit_notice(notice, 36.0, width),
            "May have been sent  ·  gs sent"
        );
    }

    #[test]
    fn removal_notice_keeps_its_answer_key_when_the_middle_clause_does_not_fit() {
        let prompt = "Remove 20 from Fell only?  ·  No undo  ·  y";
        let fitted = fit_notice(prompt, 34.0, |text| text.chars().count() as f32);
        assert_eq!(fitted, "Remove 20 from Fell only?  ·  y");
    }

    #[test]
    fn a_notice_gives_up_whole_words_of_the_subject_first() {
        let notice = "Not sent · \u{201c}Re: Sent copies in the thread\u{201d} · space a r reopen";
        let width = |text: &str| text.chars().count() as f32;
        assert_eq!(fit_notice(notice, 200.0, width), notice);
        assert_eq!(
            fit_notice(notice, 50.0, width),
            "Not sent · \u{201c}Re: Sent copies…\u{201d} · space a r reopen"
        );
        // Not even one word fits: the name is cut, the key stays.
        let fitted = fit_notice(notice, 33.0, width);
        assert!(fitted.ends_with("\u{201d} · space a r reopen"), "{fitted}");
    }

    fn person(name: &str, email: &str) -> fell_core::Participant {
        fell_core::Participant {
            name: Some(name.into()),
            email: email.into(),
        }
    }

    #[test]
    fn latest_sender_is_emphasized_and_you_are_me() {
        let thread = FolderThread {
            thread_id: ThreadId("t".into()),
            subject: "Subject".into(),
            snippet: "hello   there\nfriend".into(),
            last_date_unix: 1_700_000_000,
            message_count: 3,
            unread: true,
            participants: vec![person("Jon", "jon@x"), person("Caleb", "me@x")],
            latest_email: "jon@x".into(),
            latest_name: Some("Jon".into()),
            labels: vec!["work".into()],
            starred: true,
            has_attachment: false,
        };
        let rows = thread_rows(&[thread], "me@x");
        assert_eq!(rows[0].spans[0].text, "Jon");
        assert!(rows[0].spans[0].emphasis);
        assert_eq!(rows[0].spans[1].text, "me");
        assert!(!rows[0].spans[1].emphasis);
        assert_eq!(rows[0].snippet, "hello there friend");
        assert_eq!(rows[0].date_unix, 1_700_000_000);
        assert_eq!(relative_date(1_700_000_120, rows[0].date_unix), "2m");
    }

    #[test]
    fn future_dates_are_absolute() {
        let now = 1_700_000_000;
        assert_eq!(relative_date(now, now), "now");
        assert_eq!(relative_date(now, now - 30), "now");
        assert_eq!(relative_date(now, now - 120), "2m");
        assert_eq!(relative_date(now, now - 86_400), "Yesterday");
        assert_eq!(compact_relative_date(now, now - 86_400), "Yest.");
        let soon = now + 180;
        let tomorrow = now + 86_400;
        assert_eq!(relative_date(now, soon), month_day(now, soon, false));
        assert_eq!(
            relative_date(now, tomorrow),
            month_day(now, tomorrow, false)
        );
        assert_ne!(relative_date(now, soon), "now");
        assert_ne!(relative_date(now, tomorrow), "now");
        let last_year = now - 400 * 86_400;
        let next_year = now + 400 * 86_400;
        let (past_year, _, _, _, _, _) = local_parts(last_year);
        let (future_year, _, _, _, _, _) = local_parts(next_year);
        assert!(relative_date(now, last_year).contains(&past_year.to_string()));
        assert!(
            compact_relative_date(now, last_year)
                .ends_with(&format!("/{:02}", past_year.rem_euclid(100)))
        );
        assert!(relative_date(now, next_year).contains(&future_year.to_string()));
        assert!(!month_day(now, now, false).contains(&local_parts(now).0.to_string()));
        let mut boundary = 1_704_240_000_i64;
        for _ in 0..40 {
            let earlier = boundary - 5 * 86_400;
            let (this_year, this_month, _, _, _, _) = local_parts(boundary);
            let (that_year, _, _, _, _, _) = local_parts(earlier);
            if this_month == 1 && that_year + 1 == this_year {
                let label = relative_date(boundary, earlier);
                assert!(
                    label.contains(&that_year.to_string()),
                    "{label} omitted {that_year}"
                );
                return;
            }
            boundary += 86_400;
        }
        panic!("local calendar has no year boundary within five days");
    }

    #[test]
    fn page_stays_on_a_tall_message_until_the_next_box() {
        let spans = [(0.0, 2000.0), (2000.0, 2400.0)];
        assert_eq!(message_at_top(&spans, 0.0), Some(0));
        assert_eq!(message_at_top(&spans, 800.0), Some(0));
        assert_eq!(message_at_top(&spans, 1999.0), Some(0));
        assert_eq!(message_at_top(&spans, 2000.0), Some(1));
        assert_eq!(message_at_top(&spans, 5000.0), Some(1));
        assert_eq!(message_at_top(&[], 0.0), None);
    }

    #[test]
    fn second_page_uses_the_offset_from_the_measured_frame() {
        let scrollport = 180.0;
        let measured = -720.0;
        // Window boxes from the layout. They are not shifted by `measured`.
        let bounds = [
            (scrollport, scrollport + 1600.0),
            (scrollport + 1600.0, scrollport + 2000.0),
        ];
        // Already 720px down. Another 500px stays inside the 1600px message.
        let next = measured - 500.0;
        assert_eq!(
            message_after_page(&bounds, scrollport, measured, next),
            Some(0)
        );
        // 1000px more reaches content y 1720, the following message.
        let past = measured - 1000.0;
        assert_eq!(
            message_after_page(&bounds, scrollport, measured, past),
            Some(1)
        );
        // Mixing the new offset with boxes that still include the old offset
        // lands in the next message while the real content top is still in
        // the first one.
        let shifted = [
            (bounds[0].0 + measured, bounds[0].1 + measured),
            (bounds[1].0 + measured, bounds[1].1 + measured),
        ];
        assert_eq!(message_at_top(&shifted, scrollport - next), Some(1));
        assert_eq!(message_after_page(&[], scrollport, measured, next), None);
    }

    #[test]
    fn nudge_keeps_the_cursor_when_the_scroll_does_not_move() {
        assert_eq!(cursor_after_nudge(4, Some(0), true, false), 4);
        assert_eq!(cursor_after_nudge(4, Some(0), false, false), 4);
        assert_eq!(cursor_after_nudge(4, Some(0), true, true), 4);
        assert_eq!(cursor_after_nudge(1, Some(3), true, true), 3);
        assert_eq!(cursor_after_nudge(4, Some(1), false, true), 1);
    }

    #[test]
    fn alias_folders_share_one_picker_row() {
        // Before any listing, a stored alias shares the role's default row.
        let stored: Vec<String> = vec![
            "junk".into(),
            "Deleted".into(),
            "Archived".into(),
            "Projects".into(),
        ];
        let folders = merge_folders(&FolderRoles::default(), &stored);
        assert_eq!(
            folders,
            vec![
                "INBOX", "Sent", "Drafts", "Archive", "Spam", "Trash", "Projects"
            ]
        );

        // Roles read from those names: each folder is its role's one row.
        let roles = FolderRoles::from_names(&stored);
        let folders = merge_folders(&roles, &stored);
        let labels: Vec<String> = folders.iter().map(|name| roles.label(name)).collect();
        assert_eq!(
            labels,
            vec![
                "Inbox", "Sent", "Drafts", "Archive", "Spam", "Trash", "Projects"
            ]
        );
        assert!(folders.iter().any(|name| name == "junk"));
        assert!(!folders.iter().any(|name| name == "Spam"));
    }

    #[test]
    fn server_folder_names_show_their_role_label() {
        let roles = FolderRoles::new(vec![
            fell_core::RoleFolder {
                role: fell_core::FolderRole::Sent,
                name: "[Gmail]/Sent Mail".into(),
                raw: "[Gmail]/Sent Mail".into(),
                source: fell_core::RoleSource::Server,
            },
            fell_core::RoleFolder {
                role: fell_core::FolderRole::All,
                name: "[Gmail]/All Mail".into(),
                raw: "[Gmail]/All Mail".into(),
                source: fell_core::RoleSource::Server,
            },
        ]);
        let folders = merge_folders(&roles, &["[Gmail]/Sent Mail".into(), "Receipts".into()]);
        assert_eq!(
            folders,
            vec![
                "INBOX",
                "[Gmail]/Sent Mail",
                "Drafts",
                "[Gmail]/All Mail",
                "Spam",
                "Trash",
                "Receipts"
            ]
        );
        assert_eq!(roles.label("[Gmail]/All Mail"), "All Mail");
        assert_eq!(roles.label("Receipts"), "Receipts");
    }

    #[test]
    fn narrow_window_shows_one_pane() {
        assert_eq!(visible_panes(640.0, Pane::List, false), (true, false));
        assert_eq!(
            visible_panes(640.0, Pane::Conversation, false),
            (false, true)
        );
        assert_eq!(visible_panes(1200.0, Pane::List, false), (true, true));
        assert_eq!(
            visible_panes(1200.0, Pane::Conversation, true),
            (false, true)
        );
        assert_eq!(visible_panes(1200.0, Pane::List, true), (true, true));
    }

    #[test]
    fn an_ellipsis_follows_the_last_word() {
        assert_eq!(
            ellipsis("send me anything by Friday."),
            "send me anything by Friday\u{2026}"
        );
        assert_eq!(ellipsis("Agenda to follow; "), "Agenda to follow\u{2026}");
        assert_eq!(ellipsis("the plan \u{2014} "), "the plan\u{2026}");
        assert_eq!(ellipsis("see (files"), "see (files\u{2026}");
        assert_eq!(ellipsis("it said \u{201c}"), "it said\u{2026}");
        assert_eq!(ellipsis("(plan)"), "(plan)\u{2026}");
        assert_eq!(ellipsis(". "), "\u{2026}");
    }

    #[test]
    fn unread_and_latest_start_expanded() {
        let message = |id: &str, unread: bool, folder: &str| Message {
            account_id: AccountId("local".into()),
            headers: MessageHeaders {
                message_id: MessageId::new(id),
                proton_conversation_id: None,
                in_reply_to: None,
                references: vec![],
                from: Address {
                    name: Some("Jon".into()),
                    email: "jon@x".into(),
                },
                subject: "S".into(),
                date_unix: 10,
            },
            remote_id: RemoteId(id.into()),
            thread_id: ThreadId("t".into()),
            snippet: "snip".into(),
            body_text: Some("body".into()),
            unread,
            folders: vec![folder.into()],
            labels: vec![],
            arrived_via: vec![],
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            starred: false,
            has_attachment: false,
            archived: false,
            snooze_until: None,
            list_id: None,
            reply_to: Some(Vec::new()),
            attachment_names: Vec::new(),
            parts: None,
        };
        let messages = conversation(
            &[
                message("a", false, "INBOX"),
                message("b", false, "Sent"),
                message("c", true, "INBOX"),
            ],
            "me@x",
            &FolderRoles::default(),
            &[],
            "INBOX",
        );
        assert!(!messages[0].expanded);
        assert!(!messages[1].expanded);
        assert!(messages[2].expanded);
        assert_eq!(messages[1].folders, ["Sent"]);
        // A draft says so beside the mail it answers, but not inside
        // Drafts, where every row is a draft.
        let roles = FolderRoles::from_names(&["INBOX".into(), "Drafts".into()]);
        let thread = [message("a", false, "INBOX"), message("d", false, "Drafts")];
        let from_inbox = conversation(&thread, "me@x", &roles, &[], "INBOX");
        assert_eq!(from_inbox[1].folders, ["Draft"]);
        let from_drafts = conversation(&thread, "me@x", &roles, &[], "Drafts");
        assert_eq!(from_drafts[0].folders, ["Inbox"]);
        assert!(from_drafts[1].folders.is_empty());
    }

    fn addressed(
        from: &str,
        to: &[(&str, &str)],
        cc: &[(&str, &str)],
        bcc: &[(&str, &str)],
    ) -> Message {
        let people = |list: &[(&str, &str)]| {
            list.iter()
                .map(|(name, email)| Address {
                    name: (!name.is_empty()).then(|| name.to_string()),
                    email: email.to_string(),
                })
                .collect::<Vec<_>>()
        };
        Message {
            account_id: AccountId("local".into()),
            headers: MessageHeaders {
                message_id: MessageId::new("m"),
                proton_conversation_id: None,
                in_reply_to: None,
                references: vec![],
                from: Address {
                    name: None,
                    email: from.into(),
                },
                subject: "S".into(),
                date_unix: 10,
            },
            remote_id: RemoteId("m".into()),
            thread_id: ThreadId("t".into()),
            snippet: "snip".into(),
            body_text: Some("body".into()),
            unread: false,
            folders: vec!["INBOX".into()],
            labels: vec![],
            arrived_via: vec![],
            to: people(to),
            cc: people(cc),
            bcc: people(bcc),
            starred: false,
            has_attachment: false,
            archived: false,
            snooze_until: None,
            list_id: None,
            reply_to: Some(Vec::new()),
            attachment_names: Vec::new(),
            parts: None,
        }
    }

    fn people(line: &RecipientLine) -> Vec<(&'static str, String)> {
        line.groups
            .iter()
            .map(|group| (group.label, group.people.join(", ")))
            .collect()
    }

    #[test]
    fn recipients_show_me_and_both_groups() {
        let message = addressed(
            "ada@x",
            &[("Caleb", "me@x"), ("Bob Stone", "bob@x")],
            &[("", "carol@x")],
            &[],
        );
        let line = recipient_line(&message, "me@x").unwrap();
        assert_eq!(line.short, "to me, Bob Stone · cc carol");
        assert_eq!(line.hidden, 0);
        assert_eq!(
            people(&line),
            [
                ("to", "me <me@x>, Bob Stone <bob@x>".to_string()),
                ("cc", "carol <carol@x>".to_string())
            ]
        );
    }

    #[test]
    fn a_long_list_is_cut_after_four_names() {
        let to = [
            ("A", "a@x"),
            ("B", "b@x"),
            ("C", "c@x"),
            ("D", "d@x"),
            ("E", "e@x"),
        ];
        let message = addressed("ada@x", &to, &[("F", "f@x")], &[]);
        let line = recipient_line(&message, "me@x").unwrap();
        assert_eq!(line.short, "to A, B, C, D");
        assert_eq!(line.hidden, 2);
        assert!(people(&line)[0].1.contains("E <e@x>"));
        assert_eq!(people(&line)[1], ("cc", "F <f@x>".to_string()));
    }

    #[test]
    fn one_name_past_the_cut_is_shown_instead_of_one_more() {
        let to = [("A", "a@x"), ("B", "b@x"), ("C", "c@x"), ("D", "d@x")];
        let message = addressed("ada@x", &to, &[("E", "e@x")], &[]);
        let line = recipient_line(&message, "me@x").unwrap();
        assert_eq!(line.short, "to A, B, C, D · cc E");
        assert_eq!(line.hidden, 0);
    }

    #[test]
    fn someone_called_me_is_not_shown_as_the_person() {
        let message = addressed(
            "ada@x",
            &[("me", "other@x"), ("", "me@elsewhere"), ("Caleb", "ME@x")],
            &[],
            &[],
        );
        let line = recipient_line(&message, "me@x").unwrap();
        assert_eq!(line.short, "to other@x, me@elsewhere, me");
    }

    #[test]
    fn bcc_shows_only_on_the_persons_own_mail() {
        let bcc = [("Hidden", "hidden@x")];
        let mine = addressed("ME@x", &[("Ada", "ada@x")], &[], &bcc);
        let line = recipient_line(&mine, "me@x").unwrap();
        assert_eq!(line.short, "to Ada");
        assert_eq!(line.bcc.as_deref(), Some("bcc Hidden"));
        assert_eq!(people(&line)[1], ("bcc", "Hidden <hidden@x>".to_string()));
        // Someone else's message never shows a Bcc, even if one was stored.
        let theirs = addressed("ada@x", &[("Caleb", "me@x")], &[], &bcc);
        let line = recipient_line(&theirs, "me@x").unwrap();
        assert_eq!(line.short, "to me");
        assert_eq!(line.bcc, None);
        assert_eq!(line.groups.len(), 1);
    }

    #[test]
    fn a_bcc_is_never_folded_into_the_count() {
        let cc = [
            ("Maya", "maya@x"),
            ("Sam", "sam@x"),
            ("Priya", "priya@x"),
            ("Jon", "jon@x"),
            ("Bob", "bob@x"),
            ("Lena", "lena@x"),
        ];
        let bcc = [("Riya", "riya@x"), ("Finance", "fin@x")];
        let mine = addressed("me@x", &[("Ada", "ada@x")], &cc, &bcc);
        let line = recipient_line(&mine, "me@x").unwrap();
        assert_eq!(line.short, "to Ada · cc Maya, Sam, Priya");
        assert_eq!(line.hidden, 3);
        assert_eq!(line.bcc.as_deref(), Some("bcc Riya +1"));
    }

    #[test]
    fn the_closed_line_drops_whole_people_and_counts_them() {
        let cc = [
            ("Maya", "maya@x"),
            ("Sam", "sam@x"),
            ("Priya", "priya@x"),
            ("Jon", "jon@x"),
            ("Bob", "bob@x"),
            ("Lena", "lena@x"),
        ];
        let bcc = [("Riya", "riya@x")];
        let mine = addressed("me@x", &[("Ada", "ada@x")], &cc, &bcc);
        let line = recipient_line(&mine, "me@x").unwrap();
        // Room for everything the budget allows.
        let (text, _, hidden) = fit_recipients(&line, |_, _| true);
        assert_eq!(text, "to Ada · cc Maya, Sam, Priya");
        assert_eq!(hidden, 3);
        // A narrow pane: measure by characters, 40 in all.
        let width = |text: &str| text.chars().count();
        let (text, tail, hidden) =
            fit_recipients(&line, |names, tail| width(names) + width(tail) <= 40);
        assert_eq!(text, "to Ada · cc Maya");
        assert_eq!(tail, " +5 more · bcc Riya · q");
        assert_eq!(hidden, 5);
        // Nothing fits: the first person still shows, and the count is right.
        let (text, _, hidden) = fit_recipients(&line, |_, _| false);
        assert_eq!(text, "to Ada");
        assert_eq!(hidden, 6);
    }

    #[test]
    fn the_q_key_shows_only_when_the_groups_hold_more() {
        // Quiet: "to Ada · q" said nothing the line did not.
        let alone = addressed("me@x", &[("Ada", "ada@x")], &[], &[]);
        let line = recipient_line(&alone, "me@x").unwrap();
        assert_eq!(fit_recipients(&line, |_, _| true).1, "");
        let bcc = addressed("me@x", &[("Ada", "ada@x")], &[], &[("Riya", "riya@x")]);
        let line = recipient_line(&bcc, "me@x").unwrap();
        assert_eq!(fit_recipients(&line, |_, _| true).1, " · bcc Riya · q");
    }

    #[test]
    fn a_message_with_no_stored_recipients_has_no_line() {
        let message = addressed("ada@x", &[], &[], &[("Hidden", "hidden@x")]);
        assert_eq!(recipient_line(&message, "me@x"), None);
    }
}
