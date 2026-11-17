//! Row and conversation presentation. Pure functions, no GPUI.

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
    pub unread: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConvMessage {
    pub sender: String,
    pub email: String,
    pub folders: String,
    pub date_unix: i64,
    pub snippet: String,
    pub body: RenderedBody,
    pub expanded: bool,
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
            unread: thread.unread,
        })
        .collect()
}

pub fn conversation(messages: &[Message], me: &str) -> Vec<ConvMessage> {
    let last = messages.len().saturating_sub(1);
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let email = message.headers.from.email.clone();
            ConvMessage {
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
        return absolute_date(then);
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
        return weekday(tw).into();
    }
    format!("{} {td}", month_name(tmo))
}

fn absolute_date(unix: i64) -> String {
    let (_, month, day, _, _, _) = local_parts(unix);
    format!("{} {day}", month_name(month))
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

#[cfg(test)]
mod tests {
    use super::*;
    use fell_core::message::Address;
    use fell_core::{AccountId, MessageHeaders, MessageId, RemoteId, ThreadId};

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
        assert_eq!(relative_date(now, soon), absolute_date(soon));
        assert_eq!(relative_date(now, tomorrow), absolute_date(tomorrow));
        assert_ne!(relative_date(now, soon), "now");
        assert_ne!(relative_date(now, tomorrow), "now");
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
}
