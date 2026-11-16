//! MCP server stub for Fell.
//!
//! Planned tools: search, summarize, triage, draft. There is **no send tool**.
//! Sending mail requires [`fell_core::UserSendConfirmation`] minted by the
//! GPUI app via `fell-confirm::InteractiveUi`. This crate must not depend on
//! `fell-confirm`.

use fell_core::{AccountId, Draft, SearchHit, Store, ThreadId};

/// Agent-facing tools. Intentionally excludes send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTool {
    Search { query: String },
    Summarize { thread_id: ThreadId },
    Triage { thread_id: ThreadId },
    DraftReply { thread_id: ThreadId, body: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentToolResult {
    Search(Vec<SearchHit>),
    Summary(String),
    Triage(TriageSuggestion),
    Draft(Draft),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriageSuggestion {
    pub thread_id: ThreadId,
    pub label: String,
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error(transparent)]
    Core(#[from] fell_core::Error),
    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
}

/// Execute a read/draft tool against the local store. Never sends.
pub fn dispatch(
    _store: &Store,
    _account: &AccountId,
    tool: AgentTool,
) -> Result<AgentToolResult, McpError> {
    match tool {
        AgentTool::Search { .. } => Err(McpError::NotImplemented("mcp search")),
        AgentTool::Summarize { .. } => Err(McpError::NotImplemented("mcp summarize")),
        AgentTool::Triage { .. } => Err(McpError::NotImplemented("mcp triage")),
        AgentTool::DraftReply { .. } => Err(McpError::NotImplemented("mcp draft")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_send_variant() {
        let names = ["search", "summarize", "triage", "draft"];
        assert!(names.iter().all(|n| *n != "send"));
        let err = dispatch(
            &Store::open_in_memory().unwrap(),
            &AccountId("a".into()),
            AgentTool::Search {
                query: "invoice".into(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, McpError::NotImplemented(_)));
    }
}
