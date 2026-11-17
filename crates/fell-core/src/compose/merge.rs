//! Three-way merge of `draft.md`. The user's text wins an overlap.
//!
//! Line edits come from `similar` (Apache-2.0). An agent edit applies only
//! when it does not touch a line the user changed. Each applied edit is one
//! review hunk.

use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkKind {
    Body,
    Recipients,
    Subject,
    Attachments,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeHunk {
    pub kind: HunkKind,
    pub previous: String,
    pub inserted: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    pub text: String,
    pub hunks: Vec<MergeHunk>,
    pub skipped: Vec<String>,
}

struct LineEdit {
    base_start: usize,
    base_end: usize,
    text: String,
}

pub fn merge_draft(base: &str, ours: &str, theirs: &str) -> MergeOutcome {
    let base = normalize(base);
    let ours = normalize(ours);
    let theirs = normalize(theirs);
    if theirs == base {
        return MergeOutcome {
            text: ours,
            hunks: Vec::new(),
            skipped: Vec::new(),
        };
    }
    let user_edits = line_edits(&base, &ours);
    let agent_edits = line_edits(&base, &theirs);
    let base_lines = split_keep(&base);
    let mut our_lines = split_keep(&ours);
    let base_to_our = equal_line_map(&base, &ours, base_lines.len());
    let mut hunks = Vec::new();
    let mut skipped = Vec::new();
    for edit in agent_edits.into_iter().rev() {
        let previous = slice_lines(&base_lines, edit.base_start, edit.base_end);
        let inserted = edit.text.clone();
        if is_reply_only(&previous, &inserted) {
            skipped.push("in_reply_to is set by Fell; the edit was ignored".into());
            continue;
        }
        let overlapped = user_edits.iter().any(|user| ranges_overlap(user, &edit));
        if overlapped {
            let preview: String = inserted.chars().take(80).collect();
            skipped.push(format!(
                "kept your text; an agent edit overlapped it ({preview})"
            ));
            continue;
        }
        let Some(start) = base_to_our.get(edit.base_start).copied().flatten() else {
            skipped.push("kept your text; an agent edit overlapped it".into());
            continue;
        };
        let end = if edit.base_start == edit.base_end {
            start
        } else {
            let Some(end) = base_to_our.get(edit.base_end).copied().flatten() else {
                skipped.push("kept your text; an agent edit overlapped it".into());
                continue;
            };
            end
        };
        if end < start || end > our_lines.len() {
            skipped.push("kept your text; an agent edit overlapped it".into());
            continue;
        }
        let previous_ours = slice_lines(&our_lines, start, end);
        let replacement = split_keep(&inserted);
        our_lines.splice(start..end, replacement);
        if previous_ours != inserted {
            hunks.push(MergeHunk {
                kind: classify(&inserted),
                previous: previous_ours,
                inserted,
            });
        }
    }
    hunks.reverse();
    MergeOutcome {
        text: our_lines.concat(),
        hunks,
        skipped,
    }
}

pub fn unified_diff(before: &str, after: &str) -> String {
    if before == after {
        return String::new();
    }
    TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(2)
        .header("draft", "draft")
        .to_string()
}

/// True when `current[start..end]` sits on a line the user changed since `base`.
pub fn span_overlaps_user_change(base: &str, current: &str, start: usize, end: usize) -> bool {
    if base == current {
        return false;
    }
    let current_lines = split_keep(current);
    let mut byte = 0usize;
    let mut first = None;
    let mut last = 0usize;
    for (index, line) in current_lines.iter().enumerate() {
        let next = byte + line.len();
        if start < next && end > byte {
            if first.is_none() {
                first = Some(index);
            }
            last = index + 1;
        }
        byte = next;
    }
    let Some(first) = first else {
        return false;
    };
    let base_lines = split_keep(base);
    let map = equal_line_map(base, current, base_lines.len());
    let mut unchanged = vec![false; current_lines.len()];
    for (base_index, our_index) in map.iter().enumerate() {
        if base_index == base_lines.len() {
            continue;
        }
        if let Some(our_index) = our_index
            && *our_index < unchanged.len()
        {
            unchanged[*our_index] = true;
        }
    }
    (first..last).any(|index| !unchanged.get(index).copied().unwrap_or(false))
}

pub fn find_unique(haystack: &str, needle: &str) -> Result<usize, &'static str> {
    if needle.is_empty() {
        return Err("edit is empty");
    }
    let mut found = None;
    let mut start = 0;
    while let Some(at) = haystack[start..].find(needle) {
        let absolute = start + at;
        if found.is_some() {
            return Err("edit matched more than once; include more of the paragraph");
        }
        found = Some(absolute);
        start = absolute + needle.len();
    }
    found.ok_or("the text you tried to edit has changed")
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn line_edits(base: &str, edited: &str) -> Vec<LineEdit> {
    let diff = TextDiff::from_lines(base, edited);
    let mut edits = Vec::new();
    let mut base_line = 0usize;
    let mut current: Option<LineEdit> = None;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                if let Some(edit) = current.take() {
                    edits.push(edit);
                }
                base_line += 1;
            }
            ChangeTag::Delete => {
                let edit = current.get_or_insert_with(|| LineEdit {
                    base_start: base_line,
                    base_end: base_line,
                    text: String::new(),
                });
                edit.base_end = base_line + 1;
                base_line += 1;
            }
            ChangeTag::Insert => {
                let edit = current.get_or_insert_with(|| LineEdit {
                    base_start: base_line,
                    base_end: base_line,
                    text: String::new(),
                });
                edit.text.push_str(change.value());
            }
        }
    }
    if let Some(edit) = current {
        edits.push(edit);
    }
    edits
}

fn equal_line_map(base: &str, ours: &str, base_len: usize) -> Vec<Option<usize>> {
    let mut map = vec![None; base_len + 1];
    let diff = TextDiff::from_lines(base, ours);
    let mut base_line = 0usize;
    let mut our_line = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                if base_line < map.len() {
                    map[base_line] = Some(our_line);
                }
                base_line += 1;
                our_line += 1;
            }
            ChangeTag::Delete => base_line += 1,
            ChangeTag::Insert => our_line += 1,
        }
    }
    map[base_len] = Some(our_line);
    map
}

fn ranges_overlap(left: &LineEdit, right: &LineEdit) -> bool {
    if left.base_start == left.base_end && right.base_start == right.base_end {
        return left.base_start == right.base_start;
    }
    left.base_start < right.base_end && right.base_start < left.base_end
}

fn split_keep(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0;
    for (offset, _) in text.match_indices('\n') {
        lines.push(text[start..=offset].to_string());
        start = offset + 1;
    }
    if start < text.len() {
        lines.push(text[start..].to_string());
    }
    lines
}

fn slice_lines(lines: &[String], start: usize, end: usize) -> String {
    lines.get(start..end).unwrap_or(&[]).concat()
}

fn classify(inserted: &str) -> HunkKind {
    let mut kind = HunkKind::Body;
    for line in inserted.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("to:")
            || trimmed.starts_with("cc:")
            || trimmed.starts_with("bcc:")
            || trimmed.starts_with("from:")
        {
            return HunkKind::Recipients;
        }
        if trimmed.starts_with("subject:") {
            kind = HunkKind::Subject;
        }
        if trimmed.starts_with("attachments:")
            || trimmed.starts_with("path:")
            || trimmed.starts_with("- path:")
            || trimmed.starts_with("name:")
        {
            return HunkKind::Attachments;
        }
    }
    kind
}

fn is_reply_only(previous: &str, inserted: &str) -> bool {
    fn lines(text: &str) -> Vec<String> {
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect()
    }
    let prev = lines(previous);
    let next = lines(inserted);
    prev.len() == 1
        && next.len() == 1
        && prev[0].contains("in_reply_to:")
        && next[0].contains("in_reply_to:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disjoint_edits_both_apply() {
        let base = "---\nsubject: \"a\"\nin_reply_to: \"\"\nattachments: []\n---\nalpha\nbeta\n";
        let ours = base.replace("alpha", "ALPHA");
        let theirs = base.replace("beta", "BETA");
        let merged = merge_draft(base, &ours, &theirs);
        assert!(merged.skipped.is_empty(), "{:?}", merged.skipped);
        assert!(merged.text.contains("ALPHA"), "{}", merged.text);
        assert!(merged.text.contains("BETA"), "{}", merged.text);
        assert_eq!(merged.hunks.len(), 1);
    }

    #[test]
    fn overlap_keeps_the_user() {
        let base =
            "---\nsubject: \"a\"\nin_reply_to: \"\"\nattachments: []\n---\nHello paragraph\n";
        let ours = base.replace("Hello paragraph", "Hello USER");
        let theirs = base.replace("Hello paragraph", "Hello AGENT");
        let merged = merge_draft(base, &ours, &theirs);
        assert!(merged.text.contains("Hello USER"), "{}", merged.text);
        assert!(!merged.text.contains("Hello AGENT"));
        assert!(!merged.skipped.is_empty());
    }
}
