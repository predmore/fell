use crate::message::MessageId;
use crate::threading::ThreadId;

/// One FTS hit. The UI and MCP both consume this; they do not query SQLite themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub message_id: MessageId,
    pub thread_id: ThreadId,
    pub subject: String,
    pub snippet: String,
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
