use crate::message::MessageId;
use crate::threading::ThreadId;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_quotes() {
        assert_eq!(fts5_phrase(r#"foo "bar""#), r#""foo ""bar""""#);
    }
}
