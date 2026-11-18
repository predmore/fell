//! Row and conversation presentation. Pure functions, no GPUI.

use fell_core::compose::DraftSummary;
use fell_core::{FolderThread, Message};

use crate::text_render::{RenderedBody, render_stored_body};

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
    pub folders: String,
    pub date_unix: i64,
    pub snippet: String,
    pub body: RenderedBody,
    pub expanded: bool,
    /// Who else got the message. `None` when nothing is stored.
    pub recipients: Option<RecipientLine>,
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
        if hidden > 0 {
            format!(" +{hidden} more{bcc} · q")
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

/// Rows for compose drafts, shown above the synced Drafts folder: who the
/// draft is to, a `draft` chip (or `may have been sent`), and the agent it
/// belongs to.
pub fn draft_rows(drafts: &[DraftSummary]) -> Vec<ThreadRow> {
    drafts
        .iter()
        .map(|draft| {
            let names: Vec<String> = draft
                .to
                .iter()
                .chain(&draft.cc)
                .map(|raw| recipient_label(raw))
                .collect();
            let spans = if names.is_empty() {
                vec![NameSpan {
                    text: "No recipients".into(),
                    emphasis: false,
                }]
            } else {
                // "to Ada": the column names who a draft is to, not who
                // sent it, as it does for threads.
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
            };
            let mut labels = vec![if draft.maybe_sent {
                "check Sent".to_string()
            } else {
                "draft".to_string()
            }];
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
                snippet: one_line(&draft.body),
                labels,
                warn: draft.maybe_sent,
                unread: false,
                draft: Some(draft.id.clone()),
            }
        })
        .collect()
}

/// A draft in the reading pane: one message, from the person, as it reads.
pub fn draft_conversation(draft: &DraftSummary, me: &str) -> Vec<ConvMessage> {
    let address = |raw: &String| {
        let (name, email) = fell_core::compose::mailbox_parts(raw)
            .unwrap_or_else(|| (None, raw.trim().to_string()));
        // Named as the draft's row names them, so "to Ada" reads the same.
        let name = name
            .filter(|name| !name.trim().is_empty())
            .or_else(|| Some(recipient_label(&email)));
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
        snippet: one_line(&draft.body),
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
        reply_to: Vec::new(),
        attachment_names: Vec::new(),
    };
    let mut shown = conversation(&[message], me);
    for message in &mut shown {
        message.folders = if draft.maybe_sent {
            "Draft · may have been sent · check Sent before sending again".into()
        } else {
            "Draft".into()
        };
        message.expanded = true;
    }
    shown
}

/// A recipient as a draft row names them: the display name, or the
/// address's local part with a capital, as a name would have.
fn recipient_label(raw: &str) -> String {
    let named = fell_core::compose::mailbox_parts(raw)
        .is_some_and(|(name, _)| name.as_deref().is_some_and(|name| !name.trim().is_empty()));
    let label = address_name(raw);
    if named {
        return label;
    }
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => label,
    }
}

/// The name a draft row shows for one recipient.
fn address_name(raw: &str) -> String {
    match fell_core::compose::mailbox_parts(raw) {
        Some((name, email)) => display_name(&name, &email, ""),
        None => one_line(raw),
    }
}

pub fn conversation(messages: &[Message], me: &str) -> Vec<ConvMessage> {
    let last = messages.len().saturating_sub(1);
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let email = message.headers.from.email.clone();
            ConvMessage {
                id: message.headers.message_id.as_str().to_string(),
                sender: display_name(&message.headers.from.name, &email, me),
                email,
                folders: message
                    .folders
                    .iter()
                    .map(|folder| folder_label(folder))
                    .collect::<Vec<_>>()
                    .join(" · "),
                date_unix: message.headers.date_unix,
                snippet: one_line(&message.snippet),
                body: render_stored_body(message.body_text.as_deref(), &message.snippet),
                expanded: message.unread || index == last,
                recipients: recipient_line(message, me),
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

pub fn folder_label(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "inbox" => "Inbox".into(),
        "sent" => "Sent".into(),
        "drafts" => "Drafts".into(),
        "archive" | "archived" => "Archive".into(),
        "spam" | "junk" => "Spam".into(),
        "trash" | "deleted" => "Trash".into(),
        _ => name.to_string(),
    }
}

pub fn merge_folders(stored: &[String]) -> Vec<String> {
    let standard = ["INBOX", "Sent", "Drafts", "Archive", "Spam", "Trash"];
    let mut out: Vec<String> = standard.iter().map(|name| (*name).to_string()).collect();
    for name in stored {
        let canonical = fell_core::canonical_folder_name(name)
            .map(str::to_string)
            .unwrap_or_else(|| name.clone());
        if out.iter().any(|have| have.eq_ignore_ascii_case(&canonical)) {
            continue;
        }
        out.push(canonical);
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
    // A future timestamp is not "now". Compare first: saturating_sub never goes negative.
    if then > now {
        return month_day(now, then);
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
        return "Yesterday".into();
    }
    if delta < 7 * 24 * 60 * 60 {
        if ty != ny {
            return month_day(now, then);
        }
        return weekday(tw).into();
    }
    month_day(now, then)
}

fn month_day(now: i64, then: i64) -> String {
    let (now_year, _, _, _, _, _) = local_parts(now);
    let (year, month, day, _, _, _) = local_parts(then);
    if year != now_year {
        format!("{} {day}, {year}", month_name(month))
    } else {
        format!("{} {day}", month_name(month))
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
const NOTICE_DOT: &str = "  ·  ";

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
        let candidate = format!("{before}{}…{after}", short.trim_end());
        if measure(&candidate) <= max_px {
            best = candidate;
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use fell_core::message::Address;
    use fell_core::{AccountId, MessageHeaders, MessageId, RemoteId, ThreadId};

    #[test]
    fn a_draft_row_names_who_it_is_to() {
        assert_eq!(recipient_label("ada@example.com"), "Ada");
        assert_eq!(
            recipient_label("Ada Lovelace <ada@example.com>"),
            "Ada Lovelace"
        );
        assert_eq!(recipient_label("jon.smith@example.com"), "Jon.smith");
    }

    #[test]
    fn a_narrow_warning_drops_its_middle_clause_before_its_key() {
        let notice = "may have been sent  ·  check Sent before sending again  ·  \u{201c}Budget numbers\u{201d}  ·  gs sent";
        let width = |text: &str| text.chars().count() as f32;
        let fitted = fit_notice(notice, 60.0, width);
        assert_eq!(
            fitted,
            "may have been sent  ·  \u{201c}Budget numbers\u{201d}  ·  gs sent"
        );
        let fitted = fit_notice(notice, 48.0, width);
        assert_eq!(
            fitted,
            "may have been sent  ·  \u{201c}Budget…\u{201d}  ·  gs sent"
        );
        assert!(width(&fitted) <= 48.0);
        // Not one word of the subject fits: it goes, not a stub of it.
        assert_eq!(
            fit_notice(notice, 36.0, width),
            "may have been sent  ·  gs sent"
        );
    }

    #[test]
    fn a_notice_gives_up_whole_words_of_the_subject_first() {
        let notice = "not sent · \u{201c}Re: Sent copies in the thread\u{201d} · space a r reopen";
        let width = |text: &str| text.chars().count() as f32;
        assert_eq!(fit_notice(notice, 200.0, width), notice);
        assert_eq!(
            fit_notice(notice, 50.0, width),
            "not sent · \u{201c}Re: Sent copies…\u{201d} · space a r reopen"
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
        let soon = now + 180;
        let tomorrow = now + 86_400;
        assert_eq!(relative_date(now, soon), month_day(now, soon));
        assert_eq!(relative_date(now, tomorrow), month_day(now, tomorrow));
        assert_ne!(relative_date(now, soon), "now");
        assert_ne!(relative_date(now, tomorrow), "now");
        let last_year = now - 400 * 86_400;
        let next_year = now + 400 * 86_400;
        let (past_year, _, _, _, _, _) = local_parts(last_year);
        let (future_year, _, _, _, _, _) = local_parts(next_year);
        assert!(relative_date(now, last_year).contains(&past_year.to_string()));
        assert!(relative_date(now, next_year).contains(&future_year.to_string()));
        assert!(!month_day(now, now).contains(&local_parts(now).0.to_string()));
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
        let folders = merge_folders(&[
            "Junk".into(),
            "Deleted".into(),
            "Archived".into(),
            "Projects".into(),
        ]);
        assert_eq!(
            folders
                .iter()
                .filter(|name| folder_label(name) == "Spam")
                .count(),
            1
        );
        assert!(folders.iter().any(|name| name == "Spam"));
        assert!(!folders.iter().any(|name| name.eq_ignore_ascii_case("junk")));
        assert!(
            !folders
                .iter()
                .any(|name| name.eq_ignore_ascii_case("deleted"))
        );
        assert!(folders.iter().any(|name| name == "Projects"));
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
            reply_to: Vec::new(),
            attachment_names: Vec::new(),
        };
        let messages = conversation(
            &[
                message("a", false, "INBOX"),
                message("b", false, "Sent"),
                message("c", true, "INBOX"),
            ],
            "me@x",
        );
        assert!(!messages[0].expanded);
        assert!(!messages[1].expanded);
        assert!(messages[2].expanded);
        assert_eq!(messages[1].folders, "Sent");
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
            reply_to: Vec::new(),
            attachment_names: Vec::new(),
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
    fn a_message_with_no_stored_recipients_has_no_line() {
        let message = addressed("ada@x", &[], &[], &[("Hidden", "hidden@x")]);
        assert_eq!(recipient_line(&message, "me@x"), None);
    }
}
