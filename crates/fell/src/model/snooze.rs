use crate::keymap::KeyId;

use super::{Overlay, Session, Step};

pub(crate) const PRESETS: [&str; 3] = ["Later today", "Tomorrow morning", "Next week"];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Picker {
    pub query: String,
    pub index: usize,
    pub error: bool,
    pub opened_at: i64,
    pub selected_count: usize,
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
            selected_count: ids.len(),
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
            KeyId::Space if self.snooze.query.len() < 64 => {
                self.snooze.query.push(' ');
                self.snooze.error = false;
            }
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
    if let Some((unit, digits)) = query
        .chars()
        .next_back()
        .zip(query.get(..query.len().saturating_sub(1)))
    {
        let seconds = match unit {
            'm' => Some(60),
            'h' => Some(3600),
            'd' => Some(86_400),
            _ => None,
        };
        if let Some(seconds) = seconds
            && !digits.is_empty()
            && digits.bytes().all(|byte| byte.is_ascii_digit())
        {
            let seconds = digits.parse::<i64>().ok()?.checked_mul(seconds)?;
            return now
                .checked_add(seconds)
                .filter(|until| *until > now && *until <= 253_402_300_799);
        }
    }
    let (day, clock) = query
        .split_once(char::is_whitespace)
        .map_or((query.as_str(), ""), |(day, clock)| (day, clock.trim()));
    let local = local_tm(now)?;
    let (days, repeat, (hour, minute)) = match day {
        "today" if clock.is_empty() => return preset(now, 0),
        "tomorrow" if clock.is_empty() => return preset(now, 1),
        "tonight" if clock.is_empty() => (0, 1, (20, 0)),
        "today" | "tonight" => (0, 1, parse_clock(clock)?),
        "tomorrow" => (1, 1, parse_clock(clock)?),
        _ => match weekday_number(day) {
            Some(day) => ((day + 7 - local.tm_wday) % 7, 7, parse_clock(clock)?),
            None => (0, 1, parse_clock(&query)?),
        },
    };
    at(local, days, hour, minute)
        .filter(|candidate| *candidate > now)
        .or_else(|| at(local, days + repeat, hour, minute))
}

fn weekday_number(day: &str) -> Option<i32> {
    Some(match day {
        "sun" | "sunday" => 0,
        "mon" | "monday" => 1,
        "tue" | "tuesday" => 2,
        "wed" | "wednesday" => 3,
        "thu" | "thursday" => 4,
        "fri" | "friday" => 5,
        "sat" | "saturday" => 6,
        _ => return None,
    })
}

fn parse_clock(clock: &str) -> Option<(i32, i32)> {
    if clock == "noon" {
        return Some((12, 0));
    }
    let (digits, meridiem) = clock
        .strip_suffix("am")
        .map(|s| (s.trim_end(), Some(0)))
        .or_else(|| clock.strip_suffix("pm").map(|s| (s.trim_end(), Some(12))))
        .unwrap_or((clock, None));
    let (hours, minutes) = digits.split_once(':').unwrap_or((digits, "00"));
    if !hours.bytes().all(|b| b.is_ascii_digit())
        || !minutes.bytes().all(|b| b.is_ascii_digit())
        || minutes.len() != 2
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

/// Calendar wording for the picker and list, with the weekday on later dates.
pub(crate) fn wake_time(now: i64, until: i64) -> String {
    let Some(date) = local_tm(until) else {
        return crate::present::absolute_date(until);
    };
    let key = |date: libc::tm| (date.tm_year, date.tm_yday);
    let today = local_tm(now);
    let tomorrow = today.and_then(|date| at(date, 1, 12, 0)).and_then(local_tm);
    let day = if today.is_some_and(|today| key(today) == key(date)) {
        "Today".into()
    } else if tomorrow.is_some_and(|tomorrow| key(tomorrow) == key(date)) {
        "Tomorrow".into()
    } else {
        let weekday = crate::present::weekday(date.tm_wday as u8);
        let month = crate::present::month_name((date.tm_mon + 1) as u8);
        if today.is_some_and(|today| today.tm_year == date.tm_year) {
            format!("{weekday} {month} {}", date.tm_mday)
        } else {
            format!("{weekday} {month} {} {}", date.tm_mday, date.tm_year + 1900)
        }
    };
    format!("{day} · {:02}:{:02}", date.tm_hour, date.tm_min)
}

/// The shorter form of the date used in a transient status notice.
pub(crate) fn toast_time(now: i64, until: i64) -> String {
    let Some(date) = local_tm(until) else {
        return wake_time(now, until);
    };
    let one_week = now.saturating_add(7 * 86_400);
    if until <= one_week {
        format!(
            "{} {:02}:{:02}",
            crate::present::weekday(date.tm_wday as u8),
            date.tm_hour,
            date.tm_min
        )
    } else {
        wake_time(now, until).replace(" · ", " ")
    }
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
    fn typed_days_hours_noon_and_relative_times() {
        let now = at(local_tm(1_893_456_000).unwrap(), 0, 8, 0).unwrap();
        let local = local_tm(now).unwrap();
        for (input, days, hour, minute) in [
            ("9", 0, 9, 0),
            ("noon", 0, 12, 0),
            ("today noon", 0, 12, 0),
            ("tomorrow", 1, 9, 0),
            ("tomorrow 17:00", 1, 17, 0),
            ("tonight", 0, 20, 0),
            ("tonight 9pm", 0, 21, 0),
        ] {
            assert_eq!(typed(now, input), at(local, days, hour, minute), "{input}");
        }
        assert_eq!(typed(now, "today"), preset(now, 0));
        for (input, seconds) in [("30m", 1800), ("2h", 7200), ("3d", 259200)] {
            assert_eq!(typed(now, input), Some(now + seconds));
        }
        assert_eq!(typed(now, "0m"), None);
        assert_eq!(typed(now, "tomorrow 9:"), None);
        assert_eq!(
            wake_time(now, typed(now, "tomorrow").unwrap()),
            "Tomorrow · 09:00"
        );
    }

    #[test]
    fn dst_gap_rolls_forward_in_new_york() {
        // Use a separate process so libc's timezone never changes under parallel tests.
        const CHILD: &str = "FELL_DST_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "model::snooze::tests::dst_gap_rolls_forward_in_new_york",
                ])
                .env(CHILD, "1")
                .env("TZ", "America/New_York")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        // March 8, 2026, 00:30 EST, before the clock skips 02:30.
        let now = 1_772_947_800;
        let local = local_tm(now).unwrap();
        assert_eq!(
            (local.tm_mon, local.tm_mday, local.tm_hour, local.tm_min),
            (2, 8, 0, 30)
        );
        assert!(at(local, 0, 2, 30).is_none());
        assert_eq!(typed(now, "02:30"), at(local, 1, 2, 30));
        assert_eq!(typed(now, "sun 02:30"), at(local, 7, 2, 30));
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
