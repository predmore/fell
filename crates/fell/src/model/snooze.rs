use crate::keymap::KeyId;

use super::{Overlay, Session, Step};

pub(crate) const PRESETS: [&str; 3] = ["Later today", "Tomorrow morning", "Next week"];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Picker {
    pub query: String,
    pub index: usize,
    pub error: bool,
    pub opened_at: i64,
    pub ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TriageEdit {
    pub ids: Vec<String>,
    pub kind: TriageKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriageKind {
    Snooze(i64),
    Unsnooze,
    Mute,
}

impl Session {
    pub(super) fn show_snooze_picker(&mut self, ids: Vec<String>) {
        self.snooze = Picker {
            opened_at: crate::present::now_unix(),
            ids,
            ..Picker::default()
        };
        self.overlay = Overlay::Snooze;
    }

    pub(super) fn snooze_key(&mut self, key: KeyId) -> Step {
        match key {
            KeyId::Escape => self.overlay = Overlay::None,
            KeyId::Enter => return self.confirm_snooze(),
            KeyId::Up | KeyId::Ctrl('p') if self.snooze.query.is_empty() => {
                self.snooze.index = (self.snooze.index + PRESETS.len() - 1) % PRESETS.len();
            }
            KeyId::Down | KeyId::Ctrl('n') if self.snooze.query.is_empty() => {
                self.snooze.index = (self.snooze.index + 1) % PRESETS.len();
            }
            KeyId::Backspace => {
                self.snooze.query.pop();
                self.snooze.error = false;
            }
            KeyId::Char(ch) if self.snooze.query.len() < 64 => {
                self.snooze.query.push(ch);
                self.snooze.error = false;
            }
            KeyId::Space if self.snooze.query.len() < 64 => self.snooze.query.push(' '),
            _ => {}
        }
        Step::default()
    }

    pub(crate) fn confirm_snooze(&mut self) -> Step {
        let now = crate::present::now_unix();
        let until = if self.snooze.query.trim().is_empty() {
            preset(now, self.snooze.index)
        } else {
            typed(now, &self.snooze.query)
        };
        let Some(until) = until else {
            self.snooze.error = true;
            return Step::default();
        };
        self.overlay = Overlay::None;
        Step {
            triage: Some(TriageEdit {
                ids: std::mem::take(&mut self.snooze.ids),
                kind: TriageKind::Snooze(until),
            }),
            ..Step::default()
        }
    }
}

pub(crate) fn preset(now: i64, index: usize) -> Option<i64> {
    let local = local_tm(now)?;
    match index {
        // Keep "later today" in today even when the default 6 pm has passed.
        0 => {
            let evening = at(local, 0, 18, 0)?;
            let last_minute = at(local, 0, 23, 59)?;
            Some(evening.max((now + 3600).min(last_minute).max(now + 1)))
        }
        1 => at(local, 1, 9, 0),
        2 => at(
            local,
            (8 - local.tm_wday) % 7 + if local.tm_wday == 1 { 7 } else { 0 },
            9,
            0,
        ),
        _ => None,
    }
}

pub(crate) fn typed(now: i64, query: &str) -> Option<i64> {
    let query = query.trim().to_ascii_lowercase();
    let (weekday, clock) = match query
        .split_once(char::is_whitespace)
        .filter(|(first, _)| first.starts_with(char::is_alphabetic))
    {
        Some((day, clock)) => (
            Some(match day {
                "sun" | "sunday" => 0,
                "mon" | "monday" => 1,
                "tue" | "tuesday" => 2,
                "wed" | "wednesday" => 3,
                "thu" | "thursday" => 4,
                "fri" | "friday" => 5,
                "sat" | "saturday" => 6,
                _ => return None,
            }),
            clock.trim(),
        ),
        None => (None, query.as_str()),
    };
    let (hour, minute) = parse_clock(clock)?;
    let local = local_tm(now)?;
    let days = weekday.map_or(0, |day| (day + 7 - local.tm_wday) % 7);
    let candidate = at(local, days, hour, minute)?;
    if candidate > now {
        Some(candidate)
    } else {
        at(
            local,
            days + if weekday.is_some() { 7 } else { 1 },
            hour,
            minute,
        )
    }
}

fn parse_clock(clock: &str) -> Option<(i32, i32)> {
    let (digits, meridiem) = clock
        .strip_suffix("am")
        .map(|s| (s.trim_end(), Some(0)))
        .or_else(|| clock.strip_suffix("pm").map(|s| (s.trim_end(), Some(12))))
        .unwrap_or((clock, None));
    let (hours, minutes) = digits.split_once(':').unwrap_or((digits, "00"));
    if !hours.bytes().all(|b| b.is_ascii_digit())
        || !minutes.bytes().all(|b| b.is_ascii_digit())
        || minutes.len() != 2
        || (meridiem.is_none() && !digits.contains(':'))
    {
        return None;
    }
    let mut hour: i32 = hours.parse().ok()?;
    let minute: i32 = minutes.parse().ok()?;
    if let Some(offset) = meridiem {
        if !(1..=12).contains(&hour) {
            return None;
        }
        hour = hour % 12 + offset;
    }
    (hour < 24 && minute < 60).then_some((hour, minute))
}

fn local_tm(unix: i64) -> Option<libc::tm> {
    // SAFETY: localtime_r only writes to our tm and uses the given time_t.
    unsafe {
        let mut out = std::mem::zeroed::<libc::tm>();
        (!libc::localtime_r(&(unix as libc::time_t), &mut out).is_null()).then_some(out)
    }
}

fn at(mut local: libc::tm, days: i32, hour: i32, minute: i32) -> Option<i64> {
    local.tm_mday += days;
    local.tm_hour = hour;
    local.tm_min = minute;
    local.tm_sec = 0;
    local.tm_isdst = -1;
    // SAFETY: mktime reads and normalizes our local tm.
    let unix = unsafe { libc::mktime(&mut local) };
    // A nonexistent local time on a DST transition is normalized by libc.
    (unix > 0 && local.tm_hour == hour && local.tm_min == minute).then_some(unix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clock_times_and_rejects_invalid_times() {
        assert_eq!(parse_clock("12am"), Some((0, 0)));
        assert_eq!(parse_clock("12:45 pm"), Some((12, 45)));
        assert_eq!(parse_clock("23:05"), Some((23, 5)));
        assert_eq!(parse_clock("24:00"), None);
        assert_eq!(parse_clock("3:6pm"), None);
        assert_eq!(parse_clock("0am"), None);
    }

    #[test]
    fn typed_weekday_and_presets_resolve_to_future_local_times() {
        let now = 1_893_456_000;
        let monday = typed(now, "mon 9am").unwrap();
        let local = local_tm(monday).unwrap();
        assert_eq!((local.tm_wday, local.tm_hour, local.tm_min), (1, 9, 0));
        assert!(monday > now);
        assert!(typed(monday, "Monday 9am").unwrap() - monday >= 6 * 86_400);
        assert!((0..3).all(|preset_id| preset(now, preset_id).unwrap() > now));
        assert!(typed(now, "mon 25:00").is_none());
    }
}
