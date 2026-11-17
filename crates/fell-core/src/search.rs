//! Local-store keyword search.
//!
//! `execute` reads SQLite only. It does not call a mail backend, so a result
//! changes only when the store changes. Hits are one per thread, ordered by
//! the best BM25 score of a message that matched every term and operator.
//!
//! Exact text uses FTS5 (`unicode61`) with `bm25`. `from:`, `to:`, and
//! `subject:` are column filters. `in:`, `label:`, `has:attachment`,
//! `is:unread`, `before:`, and `after:` filter the message row. When the
//! exact query matches nothing, a character-trigram index of indexed words
//! proposes corrections and Damerau-Levenshtein confirms them. If the
//! corrected query matches, those hits are returned with `did_you_mean`.

use std::collections::HashSet;

#[cfg(test)]
use std::cell::Cell;

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params_from_iter};

use crate::account::AccountId;
use crate::message::MessageId;
use crate::threading::ThreadId;
use crate::{Error, Result};

/// One FTS hit. The UI and MCP both consume this; they do not query SQLite themselves.
///
/// Hits are grouped by thread: at most one row per conversation, ranked by the
/// best BM25 score of its messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub message_id: MessageId,
    pub thread_id: ThreadId,
    pub subject: String,
    pub snippet: String,
    /// Address only. The display name stays out of search hits.
    pub sender_email: String,
    pub date_unix: i64,
}

/// Keyword search against the local store.
///
/// `hits` is empty when nothing matches. `did_you_mean` is set only when the
/// typed query matched nothing and a trigram correction did; in that case
/// `hits` are the corrected results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
    pub did_you_mean: Option<String>,
}

/// Quote a user query as an FTS5 phrase so operators in the input are literal.
pub(crate) fn fts5_phrase(raw: &str) -> String {
    let trimmed = raw.trim();
    let escaped = trimmed.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

const TERM_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS search_terms (
    account_id TEXT NOT NULL,
    term TEXT NOT NULL,
    df INTEGER NOT NULL,
    PRIMARY KEY (account_id, term)
);

CREATE TABLE IF NOT EXISTS search_trigrams (
    account_id TEXT NOT NULL,
    trigram TEXT NOT NULL,
    term TEXT NOT NULL,
    PRIMARY KEY (account_id, trigram, term)
);
"#;

pub(crate) fn ensure_vocabulary(conn: &Connection) -> Result<()> {
    conn.execute_batch(TERM_SCHEMA)?;
    Ok(())
}

/// Fill the trigram vocabulary from messages already stored before the table existed.
///
/// Rows are read one at a time inside a transaction. If that transaction does
/// not commit, the partial vocabulary is removed so the next open retries.
pub(crate) fn rebuild_vocabulary_if_empty(conn: &Connection) -> Result<()> {
    let terms: i64 = conn.query_row("SELECT COUNT(*) FROM search_terms", [], |row| row.get(0))?;
    if terms > 0 {
        return Ok(());
    }
    let messages: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
    if messages == 0 {
        return Ok(());
    }
    let outcome = {
        let tx = conn.unchecked_transaction()?;
        match rebuild_vocabulary_rows(&tx) {
            Ok(()) => tx.commit().map_err(crate::Error::from),
            Err(err) => {
                let _ = tx.rollback();
                Err(err)
            }
        }
    };
    if let Err(err) = outcome {
        clear_vocabulary(conn)?;
        return Err(err);
    }
    Ok(())
}

fn rebuild_vocabulary_rows(conn: &Connection) -> Result<()> {
    let mut id_stmt = conn.prepare("SELECT rowid FROM messages ORDER BY rowid")?;
    let ids = id_stmt.query_map([], |row| row.get::<_, i64>(0))?;
    let mut rowids = Vec::new();
    for id in ids {
        rowids.push(id?);
    }
    drop(id_stmt);
    for (index, rowid) in rowids.into_iter().enumerate() {
        interrupt_vocabulary_rebuild(index)?;
        let row = conn.query_row(
            "SELECT account_id, subject, from_name, from_email,
                    COALESCE(body_text, snippet), to_line, cc_line, bcc_line, folders, labels
             FROM messages WHERE rowid = ?1",
            rusqlite::params![rowid],
            |row| {
                Ok(StoredText {
                    account_id: row.get(0)?,
                    subject: row.get(1)?,
                    from_name: row.get(2)?,
                    from_email: row.get(3)?,
                    body: row.get(4)?,
                    to_line: row.get(5)?,
                    cc_line: row.get(6)?,
                    bcc_line: row.get(7)?,
                    folders: row.get(8)?,
                    labels: row.get(9)?,
                })
            },
        )?;
        let terms = terms_for_indexed(IndexedText {
            subject: &row.subject,
            from_name: row.from_name.as_deref(),
            from_email: &row.from_email,
            body: &row.body,
            to_line: &row.to_line,
            cc_line: &row.cc_line,
            bcc_line: &row.bcc_line,
            folders: &row.folders,
            labels: &row.labels,
        });
        sync_terms(conn, &row.account_id, &HashSet::new(), &terms)?;
    }
    Ok(())
}

fn interrupt_vocabulary_rebuild(index: usize) -> Result<()> {
    #[cfg(test)]
    if stop_vocabulary_rebuild_at(index) {
        return Err(Error::Search("vocabulary rebuild interrupted".into()));
    }
    #[cfg(not(test))]
    let _ = index;
    Ok(())
}

fn clear_vocabulary(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM search_trigrams", [])?;
    conn.execute("DELETE FROM search_terms", [])?;
    Ok(())
}

#[cfg(test)]
thread_local! {
    static REBUILD_STOP_AT: Cell<Option<usize>> = const { Cell::new(None) };
}

#[cfg(test)]
fn stop_vocabulary_rebuild_at(index: usize) -> bool {
    REBUILD_STOP_AT.with(|cell| cell.get() == Some(index))
}

/// Test hook. `Some(n)` makes the vocabulary rebuild fail before message `n`.
#[cfg(test)]
pub(crate) fn testing_stop_vocabulary_rebuild_at(index: Option<usize>) {
    REBUILD_STOP_AT.with(|cell| cell.set(index));
}

struct StoredText {
    account_id: String,
    subject: String,
    from_name: Option<String>,
    from_email: String,
    body: String,
    to_line: String,
    cc_line: String,
    bcc_line: String,
    folders: String,
    labels: String,
}

pub(crate) struct IndexedText<'a> {
    pub subject: &'a str,
    pub from_name: Option<&'a str>,
    pub from_email: &'a str,
    pub body: &'a str,
    pub to_line: &'a str,
    pub cc_line: &'a str,
    pub bcc_line: &'a str,
    pub folders: &'a str,
    pub labels: &'a str,
}

pub(crate) fn terms_for_indexed(parts: IndexedText<'_>) -> HashSet<String> {
    let mut text = String::new();
    push_piece(&mut text, parts.subject);
    if let Some(name) = parts.from_name {
        push_piece(&mut text, name);
    }
    push_piece(&mut text, parts.from_email);
    push_piece(&mut text, parts.body);
    push_piece(&mut text, parts.to_line);
    push_piece(&mut text, parts.cc_line);
    push_piece(&mut text, parts.bcc_line);
    push_piece(&mut text, parts.folders);
    push_piece(&mut text, parts.labels);
    tokenize(&text)
}

fn push_piece(buf: &mut String, piece: &str) {
    buf.push_str(piece);
    buf.push(' ');
}

pub(crate) fn sync_terms(
    conn: &Connection,
    account_id: &str,
    old_terms: &HashSet<String>,
    new_terms: &HashSet<String>,
) -> Result<()> {
    for term in old_terms.difference(new_terms) {
        adjust_term(conn, account_id, term, -1)?;
    }
    for term in new_terms.difference(old_terms) {
        adjust_term(conn, account_id, term, 1)?;
    }
    Ok(())
}

fn adjust_term(conn: &Connection, account_id: &str, term: &str, delta: i64) -> Result<()> {
    let df: i64 = conn
        .query_row(
            "SELECT df FROM search_terms WHERE account_id = ?1 AND term = ?2",
            rusqlite::params![account_id, term],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let next = df + delta;
    if next <= 0 {
        conn.execute(
            "DELETE FROM search_terms WHERE account_id = ?1 AND term = ?2",
            rusqlite::params![account_id, term],
        )?;
        conn.execute(
            "DELETE FROM search_trigrams WHERE account_id = ?1 AND term = ?2",
            rusqlite::params![account_id, term],
        )?;
        return Ok(());
    }
    if df == 0 {
        conn.execute(
            "INSERT INTO search_terms (account_id, term, df) VALUES (?1, ?2, ?3)",
            rusqlite::params![account_id, term, next],
        )?;
        for trigram in trigrams(term) {
            conn.execute(
                "INSERT OR IGNORE INTO search_trigrams (account_id, trigram, term)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![account_id, trigram, term],
            )?;
        }
    } else {
        conn.execute(
            "UPDATE search_terms SET df = ?1 WHERE account_id = ?2 AND term = ?3",
            rusqlite::params![next, account_id, term],
        )?;
    }
    Ok(())
}

pub(crate) fn execute(
    conn: &Connection,
    account_id: &AccountId,
    query: &str,
) -> Result<SearchResults> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(SearchResults {
            hits: Vec::new(),
            did_you_mean: None,
        });
    }
    let parsed = parse_query(query)?;
    let hits = run(conn, account_id, &parsed)?;
    if !hits.is_empty() {
        return Ok(SearchResults {
            hits,
            did_you_mean: None,
        });
    }
    let Some(suggestion) = suggest_query(conn, account_id, query, &parsed)? else {
        return Ok(SearchResults {
            hits,
            did_you_mean: None,
        });
    };
    if suggestion.eq_ignore_ascii_case(query) {
        return Ok(SearchResults {
            hits: Vec::new(),
            did_you_mean: None,
        });
    }
    let reparsed = match parse_query(&suggestion) {
        Ok(parsed) => parsed,
        Err(_) => {
            return Ok(SearchResults {
                hits: Vec::new(),
                did_you_mean: None,
            });
        }
    };
    let corrected = run(conn, account_id, &reparsed)?;
    if corrected.is_empty() {
        return Ok(SearchResults {
            hits: Vec::new(),
            did_you_mean: None,
        });
    }
    Ok(SearchResults {
        hits: corrected,
        did_you_mean: Some(suggestion),
    })
}

fn run(conn: &Connection, account_id: &AccountId, parsed: &ParsedQuery) -> Result<Vec<SearchHit>> {
    if parsed.unmatched_flag {
        return Ok(Vec::new());
    }
    if let (Some(after), Some(before)) = (parsed.after, parsed.before)
        && after >= before
    {
        return Ok(Vec::new());
    }
    match fts_match(parsed) {
        Some(match_query) => run_fts(conn, account_id, parsed, &match_query),
        None => run_filtered(conn, account_id, parsed),
    }
}

fn run_fts(
    conn: &Connection,
    account_id: &AccountId,
    parsed: &ParsedQuery,
    match_query: &str,
) -> Result<Vec<SearchHit>> {
    // bm25() is only valid in the SELECT of the MATCH query, not in a window.
    let mut sql = String::from(
        "SELECT message_id, thread_id, subject, snippet, from_email, date_unix FROM (
            SELECT
                message_id,
                thread_id,
                subject,
                snippet,
                from_email,
                date_unix,
                score,
                ROW_NUMBER() OVER (
                    PARTITION BY thread_id
                    ORDER BY score ASC, date_unix DESC, message_id ASC
                ) AS rn
            FROM (
                SELECT
                    m.message_id AS message_id,
                    m.thread_id AS thread_id,
                    m.subject AS subject,
                    m.snippet AS snippet,
                    m.from_email AS from_email,
                    m.date_unix AS date_unix,
                    bm25(messages_fts, 8.0, 4.0, 2.0, 1.0) AS score
                FROM messages_fts
                JOIN messages m
                  ON m.account_id = messages_fts.account_id
                 AND m.message_id = messages_fts.message_id
                WHERE m.account_id = ? AND messages_fts MATCH ?",
    );
    let mut params = vec![
        Value::Text(account_id.as_str().to_string()),
        Value::Text(match_query.to_string()),
    ];
    append_filters(&mut sql, &mut params, parsed);
    sql.push_str(")) WHERE rn = 1 ORDER BY score ASC, thread_id ASC");
    query_hits(conn, &sql, params)
}

fn run_filtered(
    conn: &Connection,
    account_id: &AccountId,
    parsed: &ParsedQuery,
) -> Result<Vec<SearchHit>> {
    let mut sql = String::from(
        "SELECT message_id, thread_id, subject, snippet, from_email, date_unix FROM (
            SELECT
                m.message_id AS message_id,
                m.thread_id AS thread_id,
                m.subject AS subject,
                m.snippet AS snippet,
                m.from_email AS from_email,
                m.date_unix AS date_unix,
                ROW_NUMBER() OVER (
                    PARTITION BY m.thread_id
                    ORDER BY m.date_unix DESC, m.message_id ASC
                ) AS rn,
                MAX(m.date_unix) OVER (PARTITION BY m.thread_id) AS thread_date
            FROM messages m
            WHERE m.account_id = ?",
    );
    let mut params = vec![Value::Text(account_id.as_str().to_string())];
    append_filters(&mut sql, &mut params, parsed);
    sql.push_str(") WHERE rn = 1 ORDER BY thread_date DESC, thread_id ASC");
    query_hits(conn, &sql, params)
}

fn append_filters(sql: &mut String, params: &mut Vec<Value>, parsed: &ParsedQuery) {
    if parsed.is_unread {
        sql.push_str(" AND m.unread = 1");
    }
    if parsed.has_attachment {
        sql.push_str(" AND m.has_attachment = 1");
    }
    if let Some(after) = parsed.after {
        sql.push_str(" AND m.date_unix >= ?");
        params.push(Value::Integer(after));
    }
    if let Some(before) = parsed.before {
        sql.push_str(" AND m.date_unix < ?");
        params.push(Value::Integer(before));
    }
    for folder in &parsed.folders {
        push_token_filter(sql, params, "m.folders", folder);
    }
    for label in &parsed.labels {
        push_token_filter(sql, params, "m.labels", label);
    }
}

fn push_token_filter(sql: &mut String, params: &mut Vec<Value>, column: &str, value: &str) {
    sql.push_str(" AND (instr(char(30) || lower(");
    sql.push_str(column);
    sql.push_str(") || char(30), char(30) || ? || char(30)) > 0 OR instr(char(30) || ");
    sql.push_str(column);
    sql.push_str(" || char(30), char(30) || ? || char(30)) > 0)");
    params.push(Value::Text(value.to_lowercase()));
    params.push(Value::Text(value.to_string()));
}

fn query_hits(conn: &Connection, sql: &str, params: Vec<Value>) -> Result<Vec<SearchHit>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params_from_iter(params.iter()), |row| {
        Ok(SearchHit {
            message_id: MessageId(row.get(0)?),
            thread_id: ThreadId(row.get(1)?),
            subject: row.get(2)?,
            snippet: row.get(3)?,
            sender_email: row.get(4)?,
            date_unix: row.get(5)?,
        })
    })?;
    let mut hits = Vec::new();
    for row in rows {
        hits.push(row?);
    }
    Ok(hits)
}

fn fts_match(parsed: &ParsedQuery) -> Option<String> {
    let mut parts = Vec::new();
    for phrase in &parsed.text_phrases {
        parts.push(format!("({})", fts5_phrase(phrase)));
    }
    for phrase in &parsed.subject {
        parts.push(format!("(subject : {})", fts5_phrase(phrase)));
    }
    for phrase in &parsed.from {
        parts.push(format!("(sender : {})", fts5_phrase(phrase)));
    }
    for phrase in &parsed.to {
        parts.push(format!("(recipients : {})", fts5_phrase(phrase)));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" AND "))
    }
}

#[derive(Default)]
struct ParsedQuery {
    text_phrases: Vec<String>,
    from: Vec<String>,
    to: Vec<String>,
    subject: Vec<String>,
    folders: Vec<String>,
    labels: Vec<String>,
    has_attachment: bool,
    is_unread: bool,
    before: Option<i64>,
    after: Option<i64>,
    unmatched_flag: bool,
    corrections: Vec<Correction>,
}

struct Correction {
    start: usize,
    end: usize,
    kind: CorrectionKind,
}

enum CorrectionKind {
    Words,
    Flag(&'static [&'static str]),
}

fn parse_query(input: &str) -> Result<ParsedQuery> {
    let mut parsed = ParsedQuery::default();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'"' {
            let (end, phrase) = read_quoted(input, i)?;
            let phrase = phrase.trim();
            if phrase.is_empty() {
                return Err(Error::Search("empty phrase".into()));
            }
            parsed.text_phrases.push(phrase.to_string());
            parsed.corrections.push(Correction {
                start: i,
                end,
                kind: CorrectionKind::Words,
            });
            i = end;
            continue;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'"' {
            i += 1;
        }
        let token = &input[start..i];
        if let Some((key, value_in_token)) = split_operator(token) {
            if value_in_token.is_empty() && i < bytes.len() && bytes[i] == b'"' {
                let (qend, phrase) = read_quoted(input, i)?;
                let phrase = phrase.trim();
                if phrase.is_empty() {
                    return Err(Error::Search(format!("missing value for {key}:")));
                }
                let interior_start = i + 1;
                let interior_end = qend - 1;
                let correct = input.get(interior_start..interior_end) == Some(phrase);
                apply_operator(
                    &mut parsed,
                    key,
                    phrase,
                    if correct { interior_start } else { i },
                    if correct { interior_end } else { qend },
                )?;
                i = qend;
            } else if value_in_token.trim().is_empty() {
                return Err(Error::Search(format!("missing value for {key}:")));
            } else {
                let value_start = start + key.len() + 1;
                apply_operator(&mut parsed, key, value_in_token.trim(), value_start, i)?;
            }
        } else {
            parsed.text_phrases.push(token.to_string());
            parsed.corrections.push(Correction {
                start,
                end: i,
                kind: CorrectionKind::Words,
            });
        }
    }
    Ok(parsed)
}

fn split_operator(token: &str) -> Option<(&str, &str)> {
    let (key, value) = token.split_once(':')?;
    if key.is_empty() || !key.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return None;
    }
    const OPERATORS: &[&str] = &[
        "from", "to", "subject", "in", "label", "has", "is", "before", "after",
    ];
    if OPERATORS.iter().any(|op| key.eq_ignore_ascii_case(op)) {
        Some((key, value))
    } else {
        None
    }
}

fn apply_operator(
    parsed: &mut ParsedQuery,
    key: &str,
    value: &str,
    value_start: usize,
    value_end: usize,
) -> Result<()> {
    if value.is_empty() {
        return Err(Error::Search(format!("missing value for {key}:")));
    }
    match key.to_ascii_lowercase().as_str() {
        "from" => {
            parsed.from.push(value.to_string());
            push_word_correction(parsed, value_start, value_end);
        }
        "to" => {
            parsed.to.push(value.to_string());
            push_word_correction(parsed, value_start, value_end);
        }
        "subject" => {
            parsed.subject.push(value.to_string());
            push_word_correction(parsed, value_start, value_end);
        }
        "in" => {
            parsed.folders.push(value.to_string());
            push_word_correction(parsed, value_start, value_end);
        }
        "label" => {
            parsed.labels.push(value.to_string());
            push_word_correction(parsed, value_start, value_end);
        }
        "has" => {
            if value.eq_ignore_ascii_case("attachment") {
                parsed.has_attachment = true;
            } else {
                parsed.unmatched_flag = true;
            }
            parsed.corrections.push(Correction {
                start: value_start,
                end: value_end,
                kind: CorrectionKind::Flag(&["attachment"]),
            });
        }
        "is" => {
            if value.eq_ignore_ascii_case("unread") {
                parsed.is_unread = true;
            } else {
                parsed.unmatched_flag = true;
            }
            parsed.corrections.push(Correction {
                start: value_start,
                end: value_end,
                kind: CorrectionKind::Flag(&["unread"]),
            });
        }
        "before" => {
            let bound = parse_ymd(value)?;
            parsed.before = Some(match parsed.before {
                Some(existing) => existing.min(bound),
                None => bound,
            });
        }
        "after" => {
            let bound = parse_ymd(value)?;
            parsed.after = Some(match parsed.after {
                Some(existing) => existing.max(bound),
                None => bound,
            });
        }
        _ => {
            return Err(Error::Search(format!("unknown operator {key}:")));
        }
    }
    Ok(())
}

fn push_word_correction(parsed: &mut ParsedQuery, start: usize, end: usize) {
    parsed.corrections.push(Correction {
        start,
        end,
        kind: CorrectionKind::Words,
    });
}

fn read_quoted(input: &str, start: usize) -> Result<(usize, String)> {
    let bytes = input.as_bytes();
    let mut i = start + 1;
    let mut out = String::new();
    while i < bytes.len() {
        if bytes[i] == b'"' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                out.push('"');
                i += 2;
                continue;
            }
            return Ok((i + 1, out));
        }
        let ch = input[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    Err(Error::Search("unterminated quote".into()))
}

pub(crate) fn parse_ymd(value: &str) -> Result<i64> {
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return Err(Error::Search(format!("expected YYYY-MM-DD, got {value}")));
    }
    let year: i32 = parts[0]
        .parse()
        .map_err(|_| Error::Search(format!("expected YYYY-MM-DD, got {value}")))?;
    let month: u32 = parts[1]
        .parse()
        .map_err(|_| Error::Search(format!("expected YYYY-MM-DD, got {value}")))?;
    let day: u32 = parts[2]
        .parse()
        .map_err(|_| Error::Search(format!("expected YYYY-MM-DD, got {value}")))?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return Err(Error::Search(format!("expected YYYY-MM-DD, got {value}")));
    }
    Ok(civil_to_unix(year, month, day))
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Howard Hinnant's civil-from-days, inverted. `1970-01-01` is `0`.
fn civil_to_unix(year: i32, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * month_prime + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = i64::from(era) * 146097 + i64::from(doe) - 719468;
    days * 86400
}

fn suggest_query(
    conn: &Connection,
    account_id: &AccountId,
    raw: &str,
    parsed: &ParsedQuery,
) -> Result<Option<String>> {
    let mut replacements = Vec::new();
    for correction in &parsed.corrections {
        match correction.kind {
            CorrectionKind::Flag(options) => {
                let value = &raw[correction.start..correction.end];
                if let Some(suggestion) = suggest_fixed(value, options) {
                    replacements.push((correction.start, correction.end, suggestion));
                }
            }
            CorrectionKind::Words => {
                correct_words(
                    conn,
                    account_id,
                    raw,
                    correction.start,
                    correction.end,
                    &mut replacements,
                )?;
            }
        }
    }
    if replacements.is_empty() {
        return Ok(None);
    }
    replacements.sort_by_key(|(start, _, _)| *start);
    let mut out = raw.to_string();
    for (start, end, text) in replacements.into_iter().rev() {
        out.replace_range(start..end, &text);
    }
    if out == raw { Ok(None) } else { Ok(Some(out)) }
}

fn correct_words(
    conn: &Connection,
    account_id: &AccountId,
    raw: &str,
    start: usize,
    end: usize,
    replacements: &mut Vec<(usize, usize, String)>,
) -> Result<()> {
    let mut i = start;
    while i < end {
        let Some(ch) = raw[i..end].chars().next() else {
            break;
        };
        if !ch.is_alphanumeric() {
            i += ch.len_utf8();
            continue;
        }
        let word_start = i;
        let mut word = String::new();
        while i < end {
            let Some(next) = raw[i..end].chars().next() else {
                break;
            };
            if !next.is_alphanumeric() {
                break;
            }
            word.push(next);
            i += next.len_utf8();
        }
        if let Some(suggestion) = suggest_word(conn, account_id, &word)?
            && suggestion != word.to_lowercase()
        {
            replacements.push((word_start, i, suggestion));
        }
    }
    Ok(())
}

fn suggest_fixed(word: &str, options: &[&str]) -> Option<String> {
    let term = word.to_lowercase();
    if options.iter().any(|option| *option == term) {
        return None;
    }
    let max_distance = max_distance(term.chars().count());
    let mut best: Option<(usize, &str)> = None;
    for option in options {
        let distance = damerau_levenshtein(&term, option);
        if distance == 0 || distance > max_distance {
            continue;
        }
        let replace = match best {
            None => true,
            Some((best_distance, best_term)) => {
                distance < best_distance || (distance == best_distance && *option < best_term)
            }
        };
        if replace {
            best = Some((distance, option));
        }
    }
    best.map(|(_, option)| option.to_string())
}

fn suggest_word(conn: &Connection, account_id: &AccountId, word: &str) -> Result<Option<String>> {
    let term = word.to_lowercase();
    let len = term.chars().count();
    if !(3..=64).contains(&len) || term.chars().all(|ch| ch.is_ascii_digit()) {
        return Ok(None);
    }
    if term_df(conn, account_id, &term)? > 0 {
        return Ok(None);
    }
    let mut best: Option<Candidate> = None;
    if term.is_ascii() {
        let variants: Vec<String> = edit1(&term).into_iter().collect();
        for (found, df) in terms_present(conn, account_id, &variants)? {
            consider(
                &mut best,
                Candidate {
                    term: found,
                    distance: 1,
                    df,
                },
            );
        }
    }
    if best.is_none() {
        for candidate in trigram_candidates(conn, account_id, &term)? {
            let distance = damerau_levenshtein(&term, &candidate.term);
            let max_distance = max_distance(len);
            if distance == 0 || distance > max_distance {
                continue;
            }
            let query_trigrams = trigrams(&term).len();
            if distance == 1 && candidate.overlap < 1 {
                continue;
            }
            if distance == 2 && (candidate.overlap < 2 || candidate.overlap * 2 < query_trigrams) {
                continue;
            }
            consider(
                &mut best,
                Candidate {
                    term: candidate.term,
                    distance,
                    df: candidate.df,
                },
            );
        }
    }
    Ok(best.map(|candidate| candidate.term))
}

struct Candidate {
    term: String,
    distance: usize,
    df: i64,
}

fn consider(best: &mut Option<Candidate>, next: Candidate) {
    let replace = match best {
        None => true,
        Some(current) => {
            next.distance < current.distance
                || (next.distance == current.distance && next.df > current.df)
                || (next.distance == current.distance
                    && next.df == current.df
                    && next.term < current.term)
        }
    };
    if replace {
        *best = Some(next);
    }
}

fn max_distance(len: usize) -> usize {
    if len <= 4 { 1 } else { 2 }
}

fn term_df(conn: &Connection, account_id: &AccountId, term: &str) -> Result<i64> {
    let df: Option<i64> = conn
        .query_row(
            "SELECT df FROM search_terms WHERE account_id = ?1 AND term = ?2",
            rusqlite::params![account_id.as_str(), term],
            |row| row.get(0),
        )
        .optional()?;
    Ok(df.unwrap_or(0))
}

fn terms_present(
    conn: &Connection,
    account_id: &AccountId,
    words: &[String],
) -> Result<Vec<(String, i64)>> {
    let mut found = Vec::new();
    for chunk in words.chunks(200) {
        if chunk.is_empty() {
            continue;
        }
        let mut sql =
            String::from("SELECT term, df FROM search_terms WHERE account_id = ? AND term IN (");
        for (index, _) in chunk.iter().enumerate() {
            if index > 0 {
                sql.push(',');
            }
            sql.push('?');
        }
        sql.push(')');
        let mut params = Vec::with_capacity(chunk.len() + 1);
        params.push(Value::Text(account_id.as_str().to_string()));
        for word in chunk {
            params.push(Value::Text(word.clone()));
        }
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            found.push(row?);
        }
    }
    Ok(found)
}

struct TrigramHit {
    term: String,
    overlap: usize,
    df: i64,
}

fn trigram_candidates(
    conn: &Connection,
    account_id: &AccountId,
    term: &str,
) -> Result<Vec<TrigramHit>> {
    let grams = trigrams(term);
    if grams.is_empty() {
        return Ok(Vec::new());
    }
    let mut sql = String::from(
        "SELECT g.term, COUNT(*) AS overlap, t.df
         FROM search_trigrams g
         JOIN search_terms t
           ON t.account_id = g.account_id AND t.term = g.term
         WHERE g.account_id = ? AND g.trigram IN (",
    );
    for (index, _) in grams.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        sql.push('?');
    }
    sql.push_str(") GROUP BY g.term ORDER BY overlap DESC, t.df DESC LIMIT 200");
    let mut params = Vec::with_capacity(grams.len() + 1);
    params.push(Value::Text(account_id.as_str().to_string()));
    for gram in grams {
        params.push(Value::Text(gram));
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params.iter()), |row| {
        Ok(TrigramHit {
            term: row.get(0)?,
            overlap: row.get::<_, i64>(1)? as usize,
            df: row.get(2)?,
        })
    })?;
    let mut hits = Vec::new();
    for row in rows {
        hits.push(row?);
    }
    Ok(hits)
}

fn tokenize(text: &str) -> HashSet<String> {
    let mut terms = HashSet::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.push(ch);
        } else {
            push_term(&mut terms, &current);
            current.clear();
        }
    }
    push_term(&mut terms, &current);
    terms
}

fn push_term(terms: &mut HashSet<String>, token: &str) {
    let term = token.to_lowercase();
    let len = term.chars().count();
    if (3..=64).contains(&len) {
        terms.insert(term);
    }
}

fn trigrams(term: &str) -> Vec<String> {
    let chars: Vec<char> = term.chars().collect();
    if chars.len() < 3 {
        return Vec::new();
    }
    chars
        .windows(3)
        .map(|window| window.iter().collect())
        .collect()
}

fn edit1(word: &str) -> HashSet<String> {
    let chars: Vec<char> = word.chars().collect();
    let n = chars.len();
    let mut out = HashSet::new();
    for i in 0..n {
        let deleted: String = chars[..i].iter().chain(&chars[i + 1..]).collect();
        if deleted.chars().count() >= 3 {
            out.insert(deleted);
        }
    }
    for i in 0..n.saturating_sub(1) {
        let mut swapped = chars.clone();
        swapped.swap(i, i + 1);
        out.insert(swapped.into_iter().collect());
    }
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
    for i in 0..n {
        for &byte in alphabet {
            if chars[i] == byte as char {
                continue;
            }
            let mut replaced = chars.clone();
            replaced[i] = byte as char;
            out.insert(replaced.into_iter().collect());
        }
    }
    for i in 0..=n {
        for &byte in alphabet {
            let mut inserted = String::new();
            inserted.extend(chars[..i].iter());
            inserted.push(byte as char);
            inserted.extend(chars[i..].iter());
            out.insert(inserted);
        }
    }
    out.remove(word);
    out
}

fn damerau_levenshtein(left: &str, right: &str) -> usize {
    let a: Vec<char> = left.chars().collect();
    let b: Vec<char> = right.chars().collect();
    let n = a.len();
    let m = b.len();
    if n.abs_diff(m) > 2 {
        return 3;
    }
    let mut rows = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in rows[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut value = (rows[i - 1][j] + 1)
                .min(rows[i][j - 1] + 1)
                .min(rows[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                value = value.min(rows[i - 2][j - 2] + 1);
            }
            rows[i][j] = value;
        }
    }
    rows[n][m]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_quotes() {
        assert_eq!(fts5_phrase(r#"foo "bar""#), r#""foo ""bar""""#);
    }

    #[test]
    fn unix_epoch_and_leap_day() {
        assert_eq!(parse_ymd("1970-01-01").unwrap(), 0);
        assert_eq!(parse_ymd("2024-01-01").unwrap(), 1_704_067_200);
        assert!(parse_ymd("2024-02-29").is_ok());
        assert!(parse_ymd("2023-02-29").is_err());
        assert!(parse_ymd("yesterday").is_err());
    }

    #[test]
    fn transposition_is_one_edit() {
        assert_eq!(damerau_levenshtein("invoice", "invioce"), 1);
        assert_eq!(damerau_levenshtein("invoice", "invose"), 2);
    }

    #[test]
    fn parses_operators_and_phrases() {
        let parsed = parse_query(
            r#"from:ada@x.com to:"Bo Lee" subject:"march report" in:INBOX label:work has:attachment is:unread before:2024-02-01 after:2024-01-01 "pay me" leftover"#,
        )
        .unwrap();
        assert_eq!(parsed.from, vec!["ada@x.com"]);
        assert_eq!(parsed.to, vec!["Bo Lee"]);
        assert_eq!(parsed.subject, vec!["march report"]);
        assert_eq!(parsed.folders, vec!["INBOX"]);
        assert_eq!(parsed.labels, vec!["work"]);
        assert!(parsed.has_attachment);
        assert!(parsed.is_unread);
        assert_eq!(parsed.before, Some(parse_ymd("2024-02-01").unwrap()));
        assert_eq!(parsed.after, Some(parse_ymd("2024-01-01").unwrap()));
        assert_eq!(parsed.text_phrases, vec!["pay me", "leftover"]);
        assert!(!parsed.unmatched_flag);
        let Some(match_query) = fts_match(&parsed) else {
            panic!("expected an FTS query");
        };
        assert!(match_query.contains("sender :"));
        assert!(match_query.contains("recipients :"));
        assert!(match_query.contains("subject :"));
        assert!(!match_query.contains("INBOX"));
    }

    #[test]
    fn unknown_flag_is_not_an_fts_injection() {
        let parsed = parse_query("has:nope is:starred").unwrap();
        assert!(parsed.unmatched_flag);
        assert!(fts_match(&parsed).is_none());
    }

    #[test]
    fn rejects_a_bad_date_and_an_open_quote() {
        assert!(parse_query("before:2024-13-01").is_err());
        assert!(parse_query("\"open").is_err());
        assert!(parse_query("from:").is_err());
    }
}
