//! Agent changes waiting for review, and where each one sits.
//!
//! A hunk owns the byte range its text fills in the serialized draft. Every
//! later change, by the person or an agent, carries that range through a
//! diff of the text, so reject, undo, and the person's view of the draft act
//! on that range and nowhere else. Nothing searches for a hunk's text: a
//! hunk whose range no longer holds its text was overwritten, or is refused.

use std::cmp::Reverse;
use std::ops::Range;
use std::time::Duration;

use similar::{Algorithm, DiffTag, TextDiffConfig};

use super::merge::{self, HunkKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hunk {
    pub id: u64,
    pub kind: HunkKind,
    pub previous: String,
    pub inserted: String,
    /// Byte start of `inserted` in the serialized draft.
    pub at: usize,
}

impl Hunk {
    pub fn end(&self) -> usize {
        self.at + self.inserted.len()
    }

    /// `text` still has this hunk's text at its range.
    pub fn holds(&self, text: &str) -> bool {
        text.get(self.at..self.end()) == Some(self.inserted.as_str())
    }
}

/// One changed region: `old` in the earlier text became `new` in the later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Edit {
    pub old: Range<usize>,
    pub new: Range<usize>,
}

impl Edit {
    fn delta(&self) -> isize {
        self.new.len() as isize - self.old.len() as isize
    }
}

/// The regions that differ between `old` and `new`, in order.
pub(crate) fn diff(old: &str, new: &str) -> Vec<Edit> {
    if old == new {
        return Vec::new();
    }
    let prefix = common_prefix(old, new);
    let suffix = common_suffix(&old[prefix..], &new[prefix..]);
    let old_mid = &old[prefix..old.len() - suffix];
    let new_mid = &new[prefix..new.len() - suffix];
    if old_mid.is_empty() || new_mid.is_empty() {
        return vec![Edit {
            old: prefix..prefix + old_mid.len(),
            new: prefix..prefix + new_mid.len(),
        }];
    }
    // A slow diff gives up and reports a coarser change. That only makes
    // more of the text count as changed, never less.
    let mut config = TextDiffConfig::default();
    config
        .algorithm(Algorithm::Myers)
        .timeout(Duration::from_millis(50));
    let diff = config.diff_chars(old_mid, new_mid);
    let old_at = offsets(diff.iter_old_slices().map(str::len));
    let new_at = offsets(diff.iter_new_slices().map(str::len));
    let mut edits: Vec<Edit> = Vec::new();
    for op in diff.ops() {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        let old = prefix + old_at[old_range.start]..prefix + old_at[old_range.end];
        let new = prefix + new_at[new_range.start]..prefix + new_at[new_range.end];
        match edits.last_mut() {
            Some(last) if last.old.end == old.start && last.new.end == new.start => {
                last.old.end = old.end;
                last.new.end = new.end;
            }
            _ => edits.push(Edit { old, new }),
        }
    }
    edits
}

fn offsets(lens: impl Iterator<Item = usize>) -> Vec<usize> {
    let mut out = vec![0];
    let mut total = 0;
    for len in lens {
        total += len;
        out.push(total);
    }
    out
}

fn common_prefix(a: &str, b: &str) -> usize {
    let mut n = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    while !(a.is_char_boundary(n) && b.is_char_boundary(n)) {
        n -= 1;
    }
    n
}

fn common_suffix(a: &str, b: &str) -> usize {
    let mut n = a
        .bytes()
        .rev()
        .zip(b.bytes().rev())
        .take_while(|(x, y)| x == y)
        .count();
    while !(a.is_char_boundary(a.len() - n) && b.is_char_boundary(b.len() - n)) {
        n -= 1;
    }
    n
}

/// Where `hunk` starts after `edits`, or `None` when an edit rewrote its
/// text. A pure insertion or deletion that the diff placed inside the hunk
/// may belong on either side of it (`aalpha` from `alpha`); both readings
/// are tried, and the one that leaves the hunk's text in place wins.
pub(crate) fn follow(edits: &[Edit], new: &str, hunk: &Hunk) -> Option<usize> {
    let (start, end) = (hunk.at, hunk.end());
    let mut shift = 0isize;
    let mut inside = None;
    for edit in edits {
        if edit.old.end <= start {
            shift += edit.delta();
        } else if edit.old.start >= end {
            break;
        } else if inside.is_none() && start < end && (edit.old.is_empty() || edit.new.is_empty()) {
            inside = Some(edit.delta());
        } else {
            return None;
        }
    }
    let base = start as isize + shift;
    let candidates = match inside {
        Some(delta) => vec![base + delta, base],
        None => vec![base],
    };
    candidates
        .into_iter()
        .filter_map(|at| usize::try_from(at).ok())
        .find(|&at| new.get(at..at + hunk.inserted.len()) == Some(hunk.inserted.as_str()))
}

/// Carry hunks across a change the person made. A hunk whose text the
/// person rewrote is dropped: that text is theirs now. Returns how many.
pub(crate) fn follow_user(hunks: &mut Vec<Hunk>, old: &str, new: &str) -> usize {
    if hunks.is_empty() || old == new {
        return 0;
    }
    let edits = diff(old, new);
    let before = hunks.len();
    hunks.retain_mut(|hunk| match follow(&edits, new, hunk) {
        Some(at) => {
            hunk.at = at;
            true
        }
        None => false,
    });
    before - hunks.len()
}

/// Record that `old[edit.old]` became `new[edit.new]`, where
/// `edit.new.start == edit.old.start`. Hunks the edit touches fold into one
/// hunk whose `previous` is the text from before any of them, so rejecting
/// it restores the person's text rather than an earlier agent version.
///
/// With `fresh`, the change is a new agent edit and gets a hunk of its own.
/// Without it, the change is Fell rewriting its own text (re-serializing
/// the header, putting `in_reply_to` back): it only widens hunks it touches.
pub(crate) fn absorb(
    hunks: &mut Vec<Hunk>,
    old: &str,
    new: &str,
    edit: &Edit,
    fresh: Option<(u64, HunkKind)>,
) {
    debug_assert_eq!(edit.old.start, edit.new.start);
    let delta = edit.delta();
    // Fell's own rewrites fold in insertions at a hunk's edge too: quoting
    // `subject: Numbers` adds a `"` right after the agent's text.
    let edge = fresh.is_none() && edit.old.is_empty();
    let (mut touched, mut kept): (Vec<Hunk>, Vec<Hunk>) = hunks.drain(..).partition(|hunk| {
        touches(hunk.at..hunk.end(), &edit.old)
            || edge && hunk.at <= edit.old.start && edit.old.start <= hunk.end()
    });
    for hunk in &mut kept {
        if hunk.at >= edit.old.end {
            hunk.at = (hunk.at as isize + delta) as usize;
        }
    }
    if touched.is_empty() && fresh.is_none() {
        *hunks = kept;
        return;
    }
    let start = touched
        .iter()
        .map(|hunk| hunk.at)
        .chain([edit.old.start])
        .min()
        .unwrap_or(edit.old.start);
    let end = touched
        .iter()
        .map(Hunk::end)
        .chain([edit.old.end])
        .max()
        .unwrap_or(edit.old.end);
    let mut previous = old[start..end].to_string();
    touched.sort_by_key(|hunk| Reverse(hunk.at));
    for hunk in &touched {
        previous.replace_range(hunk.at - start..hunk.end() - start, &hunk.previous);
    }
    let new_end = (end as isize + delta) as usize;
    let inserted = new[start..new_end].to_string();
    let (id, mut kind) = match fresh {
        Some(fresh) => fresh,
        None => {
            let first = touched.iter().min_by_key(|hunk| hunk.id).expect("touched");
            (first.id, first.kind)
        }
    };
    for hunk in &touched {
        kind = stronger(kind, hunk.kind);
    }
    if inserted != previous {
        kept.push(Hunk {
            id,
            kind,
            previous,
            inserted,
            at: start,
        });
    }
    kept.sort_by_key(|hunk| hunk.at);
    *hunks = kept;
}

/// Carry hunks across a rewrite Fell made of its own text.
pub(crate) fn carry(hunks: &mut Vec<Hunk>, old: &str, new: &str) {
    if hunks.is_empty() || old == new {
        return;
    }
    let mut text = old.to_string();
    for edit in diff(old, new).iter().rev() {
        let step = apply(&text, &edit.old, &new[edit.new.clone()]);
        absorb(hunks, &text, &step.0, &step.1, None);
        text = step.0;
    }
    debug_assert_eq!(text, new);
}

/// `text` with `range` replaced, and the edit that describes it.
pub(crate) fn apply(text: &str, range: &Range<usize>, with: &str) -> (String, Edit) {
    let mut next = String::with_capacity(text.len() + with.len());
    next.push_str(&text[..range.start]);
    next.push_str(with);
    next.push_str(&text[range.end..]);
    let edit = Edit {
        old: range.clone(),
        new: range.start..range.start + with.len(),
    };
    (next, edit)
}

fn touches(span: Range<usize>, edit: &Range<usize>) -> bool {
    if span.is_empty() {
        return edit.start < span.start && span.start < edit.end;
    }
    if edit.is_empty() {
        return span.start < edit.start && edit.start < span.end;
    }
    span.start < edit.end && edit.start < span.end
}

fn stronger(left: HunkKind, right: HunkKind) -> HunkKind {
    fn rank(kind: HunkKind) -> u8 {
        match kind {
            HunkKind::Body => 0,
            HunkKind::Subject => 1,
            HunkKind::Attachments => 2,
            HunkKind::Recipients => 3,
        }
    }
    if rank(right) > rank(left) {
        right
    } else {
        left
    }
}

/// What part of the draft `text[start..end]` sits in: the header line it
/// changes, or the body.
pub(crate) fn kind_of(text: &str, start: usize, end: usize) -> HunkKind {
    let header_end = header_end(text);
    if start >= header_end {
        return HunkKind::Body;
    }
    let line_start = text[..start].rfind('\n').map_or(0, |index| index + 1);
    let stop = end.clamp(start, header_end);
    let line_end = text[stop..]
        .find('\n')
        .map_or(text.len(), |index| stop + index);
    merge::classify(&text[line_start..line_end.max(line_start)])
}

fn header_end(text: &str) -> usize {
    if !text.starts_with("---\n") {
        return 0;
    }
    text[4..].find("\n---\n").map_or(0, |index| 4 + index + 1)
}

pub(crate) fn to_json(hunks: &[Hunk]) -> String {
    let items: Vec<serde_json::Value> = hunks
        .iter()
        .map(|hunk| {
            serde_json::json!({
                "id": hunk.id,
                "kind": kind_name(hunk.kind),
                "previous": hunk.previous,
                "inserted": hunk.inserted,
                "at": hunk.at,
            })
        })
        .collect();
    serde_json::to_string(&items).unwrap_or_else(|_| "[]".into())
}

/// Hunks saved with `text`. One whose range does not hold its text, or that
/// overlaps another, is dropped.
pub(crate) fn from_json(json: &str, text: &str) -> Vec<Hunk> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    let mut hunks: Vec<Hunk> = items
        .iter()
        .filter_map(|item| {
            let text_of = |key: &str| item.get(key)?.as_str().map(str::to_string);
            Some(Hunk {
                id: item.get("id")?.as_u64()?,
                kind: match item.get("kind")?.as_str()? {
                    "recipients" => HunkKind::Recipients,
                    "subject" => HunkKind::Subject,
                    "attachments" => HunkKind::Attachments,
                    _ => HunkKind::Body,
                },
                previous: text_of("previous")?,
                inserted: text_of("inserted")?,
                at: usize::try_from(item.get("at")?.as_u64()?).ok()?,
            })
        })
        .filter(|hunk| hunk.previous != hunk.inserted && hunk.holds(text))
        .collect();
    hunks.sort_by_key(|hunk| hunk.at);
    let mut out: Vec<Hunk> = Vec::with_capacity(hunks.len());
    for hunk in hunks {
        let clear = out.last().is_none_or(|last| {
            last.end() <= hunk.at && !(last.at == hunk.at && last.inserted.is_empty())
        });
        if clear && out.iter().all(|seen| seen.id != hunk.id) {
            out.push(hunk);
        }
    }
    out
}

fn kind_name(kind: HunkKind) -> &'static str {
    match kind {
        HunkKind::Body => "body",
        HunkKind::Recipients => "recipients",
        HunkKind::Subject => "subject",
        HunkKind::Attachments => "attachments",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(id: u64, text: &str, inserted: &str, previous: &str) -> Hunk {
        let at = text.find(inserted).unwrap();
        Hunk {
            id,
            kind: HunkKind::Body,
            previous: previous.into(),
            inserted: inserted.into(),
            at,
        }
    }

    fn rebuild(old: &str, new: &str, edits: &[Edit]) -> String {
        let mut text = old.to_string();
        for edit in edits.iter().rev() {
            text.replace_range(edit.old.clone(), &new[edit.new.clone()]);
        }
        text
    }

    #[test]
    fn diff_edits_rebuild_the_new_text() {
        let cases = [
            ("alpha beta gamma", "alpha BETA gamma"),
            ("one\ntwo\nthree\n", "zero\none\ntwo\n3\n"),
            ("héllo wörld", "hello world!"),
            ("", "new"),
            ("old", ""),
            ("aaaa", "aaaaa"),
            ("ab ab ab", "ab xx ab ab yy"),
        ];
        for (old, new) in cases {
            let edits = diff(old, new);
            assert_eq!(rebuild(old, new, &edits), new, "{old:?} -> {new:?}");
            for pair in edits.windows(2) {
                assert!(pair[0].old.end <= pair[1].old.start);
            }
        }
    }

    #[test]
    fn a_change_before_a_hunk_moves_it() {
        let old = "intro\nagent line\nend\n";
        let mut hunks = vec![hunk(1, old, "agent line", "person line")];
        let new = "a much longer intro\nagent line\nend\n";
        assert_eq!(follow_user(&mut hunks, old, new), 0);
        assert_eq!(hunks[0].at, new.find("agent line").unwrap());
        assert!(hunks[0].holds(new));
    }

    #[test]
    fn typing_next_to_a_hunk_keeps_it() {
        // The diff places these insertions inside the hunk; they are not.
        for (old, new) in [
            ("x alpha y", "x aalpha y"),
            ("x alpha y", "x alphaa y"),
            ("x aalpha y", "x alpha y"),
        ] {
            let mut hunks = vec![Hunk {
                id: 1,
                kind: HunkKind::Body,
                previous: "beta".into(),
                inserted: "alpha".into(),
                at: old.rfind("alpha").unwrap(),
            }];
            assert_eq!(follow_user(&mut hunks, old, new), 0, "{old:?} -> {new:?}");
            assert!(hunks[0].holds(new), "{old:?} -> {new:?}: {hunks:?}");
        }
    }

    #[test]
    fn typing_inside_a_hunk_drops_it() {
        let old = "x agent words y";
        let mut hunks = vec![hunk(1, old, "agent words", "person")];
        assert_eq!(follow_user(&mut hunks, old, "x agent's words y"), 1);
        assert!(hunks.is_empty());
    }

    #[test]
    fn an_agent_edit_over_an_agent_edit_keeps_the_original_text() {
        let original = "Dear Ada, see you Monday.";
        let first = "Dear Ada, see you Tuesday.";
        let mut hunks = vec![hunk(1, first, "Tuesday", "Monday")];
        let range = first.find("you Tuesday").unwrap()..first.find('.').unwrap();
        let (second, edit) = apply(first, &range, "you on Wednesday");
        absorb(&mut hunks, first, &second, &edit, Some((2, HunkKind::Body)));
        assert_eq!(hunks.len(), 1, "{hunks:?}");
        assert!(hunks[0].holds(&second));
        let mut reverted = second.clone();
        reverted.replace_range(hunks[0].at..hunks[0].end(), &hunks[0].previous);
        assert_eq!(reverted, original);
    }

    #[test]
    fn a_rewrite_fell_makes_widens_the_hunk() {
        let old = "subject: Numbers\nbody\n";
        let mut hunks = vec![Hunk {
            id: 1,
            kind: HunkKind::Subject,
            previous: "subject: \"Hello\"".into(),
            inserted: "subject: Numbers".into(),
            at: 0,
        }];
        let new = "subject: \"Numbers\"\nbody\n";
        carry(&mut hunks, old, new);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].inserted, "subject: \"Numbers\"");
        assert!(hunks[0].holds(new));
    }

    #[test]
    fn a_rewrite_back_to_the_previous_text_clears_the_hunk() {
        let old = "in_reply_to: \"<evil@example.com>\"\n";
        let mut hunks = vec![Hunk {
            id: 1,
            kind: HunkKind::Body,
            previous: "in_reply_to: \"<abc@example.com>\"".into(),
            inserted: "in_reply_to: \"<evil@example.com>\"".into(),
            at: 0,
        }];
        carry(&mut hunks, old, "in_reply_to: \"<abc@example.com>\"\n");
        assert!(hunks.is_empty(), "{hunks:?}");
    }

    #[test]
    fn saved_hunks_that_do_not_match_the_text_are_dropped() {
        let text = "hello agent world";
        let good = hunk(1, text, "agent", "person");
        let mut wrong_place = good.clone();
        wrong_place.id = 2;
        wrong_place.at = 0;
        let mut overlap = good.clone();
        overlap.id = 3;
        overlap.inserted = "agent world".into();
        let json = to_json(&[good.clone(), wrong_place, overlap]);
        assert_eq!(from_json(&json, text), vec![good]);
        assert!(from_json("not json", text).is_empty());
    }

    #[test]
    fn header_edits_are_classified_by_their_line() {
        let text = "---\nfrom: a@b.c\nto: [\"x@y.z\"]\nsubject: \"s\"\n---\nto: body\n";
        let at = text.find("x@y.z").unwrap();
        assert_eq!(kind_of(text, at, at + 5), HunkKind::Recipients);
        let at = text.find("\"s\"").unwrap();
        assert_eq!(kind_of(text, at, at + 3), HunkKind::Subject);
        let at = text.rfind("to: body").unwrap();
        assert_eq!(kind_of(text, at, at + 2), HunkKind::Body);
    }
}
