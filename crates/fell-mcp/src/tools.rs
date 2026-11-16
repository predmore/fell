//! MCP tool registry. There is no send tool, and names containing "send"
//! are refused even if a later edit adds one to the list.

use fell_core::{AgentGrant, Message, MessageId, ReviewDraft, ReviewDraftInput, ThreadId};
use serde_json::{Map, Value, json};

use crate::protocol::Session;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolScope {
    Read,
    Triage,
    Draft,
    Undo,
}

pub(crate) struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub scope: ToolScope,
    pub schema: Value,
}

pub(crate) enum CallResult {
    Tool(Value),
    UnknownTool,
}

#[cfg(test)]
pub(crate) fn all_tool_names() -> Vec<&'static str> {
    all_tools().into_iter().map(|tool| tool.name).collect()
}

pub(crate) fn visible(grant: &AgentGrant) -> Vec<Tool> {
    all_tools()
        .into_iter()
        .filter(|tool| !is_send_name(tool.name) && scope_ok(grant, tool.scope))
        .collect()
}

pub(crate) fn call(session: &Session, name: &str, args: &Value) -> CallResult {
    if is_send_name(name) {
        return CallResult::UnknownTool;
    }
    let Some(scope) = scope_of(name) else {
        return CallResult::UnknownTool;
    };
    if let Err(err) = authorize(session, scope) {
        return CallResult::Tool(tool_err(err));
    }
    let result = dispatch(session, name, args);
    CallResult::Tool(match result {
        Ok(body) => tool_ok(body),
        Err(err) => tool_err(err),
    })
}

fn dispatch(session: &Session, name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "search" => search(session, args),
        "read_thread" => read_thread(session, args),
        "list_labels" => list_labels(session),
        "label" => label(session, args),
        "archive" => archive(session, args),
        "mark_read" => mark_read(session, args),
        "snooze" => snooze(session, args),
        "create_draft" => create_draft(session, args),
        "list_drafts" => list_drafts(session),
        "undo" => undo(session),
        _ => Err(format!("unknown tool {name}")),
    }
}

fn search(session: &Session, args: &Value) -> Result<Value, String> {
    let query = require_string(args, "query")?;
    let hits = session
        .store
        .search(&session.account_id, &query)
        .map_err(|err| err.to_string())?;
    let hits = hits
        .hits
        .iter()
        .map(|hit| {
            json!({
                "message_id": hit.message_id.as_str(),
                "thread_id": hit.thread_id.as_str(),
                "subject": hit.subject,
                "snippet": hit.snippet,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "hits": hits }))
}

fn read_thread(session: &Session, args: &Value) -> Result<Value, String> {
    let thread_id = ThreadId(require_string(args, "thread_id")?);
    let messages = session
        .store
        .thread_messages(&session.account_id, &thread_id)
        .map_err(|err| err.to_string())?;
    if messages.is_empty() {
        return Err(format!("unknown thread {}", thread_id.as_str()));
    }
    let messages = messages.iter().map(message_json).collect::<Vec<_>>();
    Ok(json!({
        "thread_id": thread_id.as_str(),
        "messages": messages,
    }))
}

fn list_labels(session: &Session) -> Result<Value, String> {
    let labels = session
        .store
        .list_labels(&session.account_id)
        .map_err(|err| err.to_string())?;
    Ok(json!({ "labels": labels }))
}

fn label(session: &Session, args: &Value) -> Result<Value, String> {
    let thread_id = ThreadId(require_string(args, "thread_id")?);
    let label = require_string(args, "label")?;
    let mode = require_string(args, "mode")?;
    let actor = actor_name(session);
    let action = match mode.as_str() {
        "add" => session
            .store
            .add_thread_label(&session.account_id, &thread_id, &label, &actor),
        "remove" => {
            session
                .store
                .remove_thread_label(&session.account_id, &thread_id, &label, &actor)
        }
        _ => return Err("mode must be add or remove".into()),
    }
    .map_err(|err| err.to_string())?;
    Ok(mutation_json(action))
}

fn archive(session: &Session, args: &Value) -> Result<Value, String> {
    let thread_id = ThreadId(require_string(args, "thread_id")?);
    let action = session
        .store
        .agent_archive_thread(&session.account_id, &thread_id, &actor_name(session))
        .map_err(|err| err.to_string())?;
    Ok(mutation_json(action))
}

fn mark_read(session: &Session, args: &Value) -> Result<Value, String> {
    let thread_id = ThreadId(require_string(args, "thread_id")?);
    let action = session
        .store
        .mark_thread_read(&session.account_id, &thread_id, &actor_name(session))
        .map_err(|err| err.to_string())?;
    Ok(mutation_json(action))
}

fn snooze(session: &Session, args: &Value) -> Result<Value, String> {
    let thread_id = ThreadId(require_string(args, "thread_id")?);
    let until_unix = require_i64(args, "until_unix")?;
    let action = session
        .store
        .snooze_thread(
            &session.account_id,
            &thread_id,
            until_unix,
            &actor_name(session),
        )
        .map_err(|err| err.to_string())?;
    Ok(mutation_json(action))
}

fn create_draft(session: &Session, args: &Value) -> Result<Value, String> {
    let thread_id = optional_string(args, "thread_id")?
        .filter(|value| !value.trim().is_empty())
        .map(ThreadId);
    let in_reply_to = optional_string(args, "in_reply_to")?
        .filter(|value| !value.trim().is_empty())
        .map(MessageId::new);
    let saved = session
        .store
        .create_review_draft(
            ReviewDraftInput {
                account_id: session.account_id.clone(),
                to: string_list(args, "to", true)?,
                cc: string_list(args, "cc", false)?,
                bcc: string_list(args, "bcc", false)?,
                subject: optional_string(args, "subject")?.unwrap_or_default(),
                body: optional_string(args, "body")?.unwrap_or_default(),
                in_reply_to,
                thread_id,
            },
            &actor_name(session),
        )
        .map_err(|err| err.to_string())?;
    Ok(json!({
        "draft_id": saved.draft.id,
        "status": "review",
        "subject": saved.draft.subject,
        "action_id": saved.action.id,
        "summary": saved.action.summary,
    }))
}

fn list_drafts(session: &Session) -> Result<Value, String> {
    let drafts = session
        .store
        .list_review_drafts(&session.account_id)
        .map_err(|err| err.to_string())?;
    let drafts = drafts.iter().map(draft_json).collect::<Vec<_>>();
    Ok(json!({ "drafts": drafts }))
}

fn undo(session: &Session) -> Result<Value, String> {
    let actor = actor_name(session);
    let latest = session
        .store
        .latest_open_action(&session.account_id, &actor)
        .map_err(|err| err.to_string())?;
    let Some(latest) = latest else {
        return Err("nothing to undo".into());
    };
    let scope = if latest.kind == "draft" {
        ToolScope::Draft
    } else {
        ToolScope::Triage
    };
    authorize(session, scope)?;
    let action = session
        .store
        .undo_actor(&session.account_id, &actor)
        .map_err(|err| err.to_string())?;
    Ok(json!({
        "undone": true,
        "action_id": action.id,
        "kind": action.kind,
        "summary": action.summary,
    }))
}

fn mutation_json(action: Option<fell_core::RecordedAction>) -> Value {
    match action {
        None => json!({ "changed": false }),
        Some(action) => json!({
            "changed": true,
            "action_id": action.id,
            "kind": action.kind,
            "summary": action.summary,
        }),
    }
}

fn message_json(message: &Message) -> Value {
    json!({
        "message_id": message.headers.message_id.as_str(),
        "from": {
            "name": message.headers.from.name,
            "email": message.headers.from.email,
        },
        "subject": message.headers.subject,
        "date_unix": message.headers.date_unix,
        "unread": message.unread,
        "folders": message.folders,
        "labels": message.labels,
        "archived": message.archived,
        "snooze_until": message.snooze_until,
        "snippet": message.snippet,
        "body": message.body_text,
    })
}

fn draft_json(draft: &ReviewDraft) -> Value {
    json!({
        "draft_id": draft.id,
        "status": "review",
        "to": draft.to,
        "cc": draft.cc,
        "bcc": draft.bcc,
        "subject": draft.subject,
        "body": draft.body,
        "in_reply_to": draft.in_reply_to.as_ref().map(|id| id.as_str()),
        "thread_id": draft.thread_id.as_ref().map(|id| id.as_str()),
        "created_unix": draft.created_unix,
    })
}

fn actor_name(session: &Session) -> String {
    format!("agent:{}", session.agent_id)
}

fn authorize(session: &Session, scope: ToolScope) -> Result<(), String> {
    let grant = session
        .store
        .agent_grant(&session.account_id, &session.agent_id)
        .map_err(|err| err.to_string())?;
    if !grant.enabled {
        return Err(format!("agent {} is disabled", session.agent_id));
    }
    if scope_ok(&grant, scope) {
        return Ok(());
    }
    Err(format!(
        "agent {} lacks the {} scope",
        session.agent_id,
        scope_name(scope)
    ))
}

fn scope_ok(grant: &AgentGrant, scope: ToolScope) -> bool {
    if !grant.enabled {
        return false;
    }
    match scope {
        ToolScope::Read => grant.read,
        ToolScope::Triage => grant.triage,
        ToolScope::Draft => grant.draft,
        ToolScope::Undo => grant.triage || grant.draft,
    }
}

fn scope_name(scope: ToolScope) -> &'static str {
    match scope {
        ToolScope::Read => "read",
        ToolScope::Triage => "triage",
        ToolScope::Draft => "draft",
        ToolScope::Undo => "triage or draft",
    }
}

fn scope_of(name: &str) -> Option<ToolScope> {
    all_tools()
        .into_iter()
        .find(|tool| tool.name == name)
        .map(|tool| tool.scope)
}

fn is_send_name(name: &str) -> bool {
    name == "submit_draft" || name.contains("send")
}

pub(crate) fn tool_ok(body: Value) -> Value {
    let text = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    })
}

pub(crate) fn tool_err(message: impl std::fmt::Display) -> Value {
    json!({
        "content": [{ "type": "text", "text": message.to_string() }],
        "isError": true,
    })
}

fn require_string(args: &Value, key: &str) -> Result<String, String> {
    match args.get(key) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("{key} must be a string")),
        None => Err(format!("{key} is required")),
    }
}

fn optional_string(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn require_i64(args: &Value, key: &str) -> Result<i64, String> {
    match args.get(key) {
        Some(Value::Number(number)) => number
            .as_i64()
            .ok_or_else(|| format!("{key} must be an integer")),
        Some(_) => Err(format!("{key} must be an integer")),
        None => Err(format!("{key} is required")),
    }
}

fn string_list(args: &Value, key: &str, required: bool) -> Result<Vec<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) if !required => Ok(Vec::new()),
        None | Some(Value::Null) => Err(format!("{key} is required")),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let Some(text) = item.as_str() else {
                    return Err(format!("{key} must be an array of strings"));
                };
                out.push(text.to_string());
            }
            if required && out.is_empty() {
                return Err(format!("{key} is required"));
            }
            Ok(out)
        }
        Some(_) => Err(format!("{key} must be an array of strings")),
    }
}

fn object_schema(properties: Map<String, Value>, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": required,
        "additionalProperties": false,
    })
}

fn prop(schema_type: &str, description: &str) -> Value {
    json!({ "type": schema_type, "description": description })
}

fn all_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "search",
            description: "Search this account's local mailbox. Literal phrase match on subject, sender, and body. Returns thread ids.",
            scope: ToolScope::Read,
            schema: object_schema(
                Map::from_iter([(
                    "query".into(),
                    prop("string", "Words to match in subject, sender, or body."),
                )]),
                &["query"],
            ),
        },
        Tool {
            name: "read_thread",
            description: "Read one thread: headers, labels, folders, archive and snooze state, and body text.",
            scope: ToolScope::Read,
            schema: object_schema(
                Map::from_iter([(
                    "thread_id".into(),
                    prop("string", "Thread id from search or a previous read."),
                )]),
                &["thread_id"],
            ),
        },
        Tool {
            name: "list_labels",
            description: "List label names used on this account.",
            scope: ToolScope::Read,
            schema: object_schema(Map::new(), &[]),
        },
        Tool {
            name: "label",
            description: "Add or remove a label on every message in a thread. Logged and undoable. Does not send mail.",
            scope: ToolScope::Triage,
            schema: object_schema(
                Map::from_iter([
                    ("thread_id".into(), prop("string", "Thread to relabel.")),
                    ("label".into(), prop("string", "Label name.")),
                    (
                        "mode".into(),
                        json!({
                            "type": "string",
                            "enum": ["add", "remove"],
                            "description": "Whether to add or remove the label.",
                        }),
                    ),
                ]),
                &["thread_id", "label", "mode"],
            ),
        },
        Tool {
            name: "archive",
            description: "Archive a thread: set the archive bit and remove INBOX. Logged and undoable.",
            scope: ToolScope::Triage,
            schema: object_schema(
                Map::from_iter([("thread_id".into(), prop("string", "Thread to archive."))]),
                &["thread_id"],
            ),
        },
        Tool {
            name: "mark_read",
            description: "Mark every message in a thread read. Logged and undoable.",
            scope: ToolScope::Triage,
            schema: object_schema(
                Map::from_iter([("thread_id".into(), prop("string", "Thread to mark read."))]),
                &["thread_id"],
            ),
        },
        Tool {
            name: "snooze",
            description: "Snooze a thread until a unix timestamp. Logged and undoable.",
            scope: ToolScope::Triage,
            schema: object_schema(
                Map::from_iter([
                    ("thread_id".into(), prop("string", "Thread to snooze.")),
                    (
                        "until_unix".into(),
                        prop("integer", "Unix seconds when the snooze ends."),
                    ),
                ]),
                &["thread_id", "until_unix"],
            ),
        },
        Tool {
            name: "create_draft",
            description: "Save a draft in Fell for a person to review. Does not send mail and cannot confirm a send.",
            scope: ToolScope::Draft,
            schema: object_schema(
                Map::from_iter([
                    (
                        "to".into(),
                        json!({
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "To addresses.",
                        }),
                    ),
                    (
                        "cc".into(),
                        json!({
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Cc addresses.",
                        }),
                    ),
                    (
                        "bcc".into(),
                        json!({
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Bcc addresses.",
                        }),
                    ),
                    ("subject".into(), prop("string", "Draft subject.")),
                    ("body".into(), prop("string", "Draft body.")),
                    (
                        "in_reply_to".into(),
                        prop("string", "Message-ID this draft replies to."),
                    ),
                    (
                        "thread_id".into(),
                        prop("string", "Thread the draft belongs to, when it is a reply."),
                    ),
                ]),
                &["to"],
            ),
        },
        Tool {
            name: "list_drafts",
            description: "List drafts saved for review in Fell. These have not been sent.",
            scope: ToolScope::Draft,
            schema: object_schema(Map::new(), &[]),
        },
        Tool {
            name: "undo",
            description: "Undo this agent's latest triage action or review draft on this account.",
            scope: ToolScope::Undo,
            schema: object_schema(Map::new(), &[]),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_no_send_tool() {
        let names = all_tool_names();
        assert_eq!(
            names,
            [
                "search",
                "read_thread",
                "list_labels",
                "label",
                "archive",
                "mark_read",
                "snooze",
                "create_draft",
                "list_drafts",
                "undo",
            ]
        );
        assert!(names.iter().all(|name| !is_send_name(name)));
        assert!(is_send_name("send"));
        assert!(is_send_name("confirm_send"));
        assert!(is_send_name("submit_draft"));
    }

    #[test]
    fn disabled_grant_lists_nothing() {
        assert!(visible(&AgentGrant::DISABLED).is_empty());
        let read_only = visible(&AgentGrant::enabled(true, false, false));
        let names: Vec<_> = read_only.iter().map(|tool| tool.name).collect();
        assert_eq!(names, ["search", "read_thread", "list_labels"]);
    }
}
