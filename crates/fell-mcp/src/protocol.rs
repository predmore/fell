//! Newline-delimited JSON-RPC for MCP stdio.
//!
//! Tool failures come back as a tool result with `isError`. A name that is
//! not in the registry, including anything that would send mail, is a
//! JSON-RPC error so it never looks like a tool Fell supports.

use std::io::{BufRead, Write};

use fell_core::{AccountId, Store};
use serde_json::{Value, json};

use crate::tools;

const PROTOCOL_VERSION: &str = "2024-11-05";
const SUPPORTED_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

pub struct Session {
    pub(crate) store: Store,
    pub(crate) account_id: AccountId,
    pub(crate) agent_id: String,
    initialized: bool,
}

impl Session {
    pub fn new(
        store: Store,
        account_id: AccountId,
        agent_id: impl Into<String>,
    ) -> Result<Self, String> {
        let agent_id = agent_id.into();
        if agent_id.is_empty()
            || agent_id != agent_id.trim()
            || agent_id.chars().any(|c| c.is_control() || c == '\u{1e}')
        {
            return Err("agent id is empty or contains a control character".into());
        }
        Ok(Self {
            store,
            account_id,
            agent_id,
            initialized: false,
        })
    }

    /// Handle one JSON-RPC message. Notifications return `None`.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let value: Value = match serde_json::from_str(line.trim()) {
            Ok(value) => value,
            Err(_) => return Some(rpc_error(&Value::Null, -32700, "parse error")),
        };
        let Some(obj) = value.as_object() else {
            return Some(rpc_error(&Value::Null, -32600, "invalid request"));
        };
        let has_id = obj.contains_key("id");
        let id = obj.get("id").cloned().unwrap_or(Value::Null);
        let method = obj.get("method").and_then(Value::as_str);
        let jsonrpc_ok = obj.get("jsonrpc").and_then(Value::as_str) == Some("2.0");
        if !has_id {
            // Notifications, including `notifications/initialized`, have no
            // response. Tools are accepted once `initialize` has succeeded.
            return None;
        }
        let Some(method) = method else {
            return Some(rpc_error(&id, -32600, "invalid request"));
        };
        if !jsonrpc_ok {
            return Some(rpc_error(&id, -32600, "invalid request"));
        }
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        Some(self.handle_request(method, &id, &params))
    }

    pub fn serve<R: BufRead, W: Write>(
        &mut self,
        mut reader: R,
        mut writer: W,
    ) -> std::io::Result<()> {
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            if line.trim().is_empty() {
                continue;
            }
            if let Some(response) = self.handle_line(line.trim_end_matches(['\r', '\n'])) {
                writer.write_all(response.as_bytes())?;
                writer.write_all(b"\n")?;
                writer.flush()?;
            }
        }
    }

    fn handle_request(&mut self, method: &str, id: &Value, params: &Value) -> String {
        if method == "initialize" {
            return self.initialize(id, params);
        }
        if !self.initialized {
            return rpc_error(id, -32600, "server not initialized");
        }
        match method {
            "ping" => rpc_result(id, json!({})),
            "tools/list" => self.tools_list(id),
            "tools/call" => self.tools_call(id, params),
            _ => rpc_error(id, -32601, "method not found"),
        }
    }

    fn initialize(&mut self, id: &Value, params: &Value) -> String {
        if self.initialized {
            return rpc_error(id, -32600, "already initialized");
        }
        let Some(obj) = params.as_object() else {
            return rpc_error(id, -32602, "invalid params");
        };
        let Some(requested) = obj.get("protocolVersion").and_then(Value::as_str) else {
            return rpc_error(id, -32602, "protocolVersion is required");
        };
        let version = if SUPPORTED_VERSIONS.contains(&requested) {
            requested
        } else {
            PROTOCOL_VERSION
        };
        self.initialized = true;
        rpc_result(
            id,
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "fell-mcp",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": self.instructions(),
            }),
        )
    }

    fn tools_list(&self, id: &Value) -> String {
        let grant = match self.store.agent_grant(&self.account_id, &self.agent_id) {
            Ok(grant) => grant,
            Err(err) => return rpc_error(id, -32603, &err.to_string()),
        };
        let tools = tools::visible(&grant)
            .into_iter()
            .filter(|tool| !tool.name.contains("send"))
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": tool.schema,
                })
            })
            .collect::<Vec<_>>();
        rpc_result(id, json!({ "tools": tools }))
    }

    fn tools_call(&self, id: &Value, params: &Value) -> String {
        let Some(obj) = params.as_object() else {
            return rpc_error(id, -32602, "invalid params");
        };
        let Some(name) = obj.get("name").and_then(Value::as_str) else {
            return rpc_error(id, -32602, "tool name is required");
        };
        let arguments = match obj.get("arguments") {
            None | Some(Value::Null) => Value::Object(serde_json::Map::new()),
            Some(Value::Object(map)) => Value::Object(map.clone()),
            Some(_) => return rpc_error(id, -32602, "arguments must be an object"),
        };
        match tools::call(self, name, &arguments) {
            tools::CallResult::Tool(value) => rpc_result(id, value),
            tools::CallResult::UnknownTool => rpc_error(id, -32602, "unknown tool"),
        }
    }

    fn instructions(&self) -> String {
        let mut text = format!(
            "Fell agent `{}` on account `{}`. Tools search and read the local mailbox, list labels, triage (label, archive, mark read, snooze), and save drafts for a person to review in Fell. There is no send tool. Mail leaves the machine only from the Fell window. ",
            self.agent_id,
            self.account_id.as_str()
        );
        match self.store.agent_grant(&self.account_id, &self.agent_id) {
            Ok(grant) if grant.enabled => {
                let mut scopes = Vec::new();
                if grant.read {
                    scopes.push("read");
                }
                if grant.triage {
                    scopes.push("triage");
                }
                if grant.draft {
                    scopes.push("draft");
                }
                if scopes.is_empty() {
                    text.push_str("The agent is enabled with no scopes.");
                } else {
                    text.push_str("Enabled scopes: ");
                    text.push_str(&scopes.join(", "));
                    text.push('.');
                }
            }
            Ok(_) => {
                text.push_str("This agent is disabled. Enable it with `fell-mcp grant`.");
            }
            Err(fell_core::Error::UnknownAccount(_)) => {
                text.push_str("That account is not in the local store.");
            }
            Err(_) => {
                text.push_str("The local store could not be read.");
            }
        }
        text
    }
}

fn rpc_result(id: &Value, result: Value) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    }))
    .unwrap_or_else(|_| internal_error())
}

fn rpc_error(id: &Value, code: i32, message: &str) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    }))
    .unwrap_or_else(|_| internal_error())
}

fn internal_error() -> String {
    r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"internal error"}}"#.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fell_core::message::Address;
    use fell_core::{
        Account, AgentGrant, BackendKind, Message, MessageHeaders, MessageId, RemoteId, ThreadId,
    };
    use serde_json::json;

    fn account(id: &str) -> Account {
        Account {
            id: AccountId(id.into()),
            display_name: "Caleb".into(),
            email: format!("{id}@x"),
            backend_kind: BackendKind::Bridge,
        }
    }

    fn message(account_id: &str, id: &str, subject: &str, body: &str, label: &str) -> Message {
        Message {
            account_id: AccountId(account_id.into()),
            headers: MessageHeaders {
                message_id: MessageId::new(id),
                proton_conversation_id: None,
                in_reply_to: None,
                references: vec![],
                from: Address {
                    name: None,
                    email: "ada@x".into(),
                },
                subject: subject.into(),
                date_unix: 30,
            },
            remote_id: RemoteId(format!("{account_id}:{id}")),
            thread_id: ThreadId::from_message_id(&MessageId::new(id)),
            snippet: body.into(),
            body_text: Some(body.into()),
            unread: true,
            folders: vec!["INBOX".into(), "Work".into()],
            labels: if label.is_empty() {
                Vec::new()
            } else {
                vec![label.into()]
            },
            arrived_via: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            has_attachment: false,
            archived: false,
            snooze_until: None,
        }
    }

    fn session_with(grant: AgentGrant, agent: &str) -> (Session, ThreadId) {
        let store = Store::open_in_memory().unwrap();
        let acct = account("a1");
        store.upsert_account(&acct).unwrap();
        let other = account("a2");
        store.upsert_account(&other).unwrap();
        let thread = store
            .upsert_message(&message(
                "a1",
                "root@x",
                "Invoice March",
                "please pay the invoice",
                "billing",
            ))
            .unwrap();
        store
            .upsert_message(&message(
                "a2",
                "secret@x",
                "Secret",
                "hidden label body",
                "secret",
            ))
            .unwrap();
        if grant != AgentGrant::DISABLED {
            store.set_agent_grant(&acct.id, agent, grant).unwrap();
        }
        let session = Session::new(store, acct.id, agent).unwrap();
        (session, thread)
    }

    fn rpc(session: &mut Session, request: Value) -> Value {
        let line = serde_json::to_string(&request).unwrap();
        let raw = session.handle_line(&line).expect("response");
        assert!(!raw.contains('\n'), "{raw}");
        serde_json::from_str(&raw).unwrap()
    }

    fn init(session: &mut Session) {
        let response = rpc(
            session,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" }
                }
            }),
        );
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["result"]["serverInfo"]["name"], "fell-mcp");
        assert!(
            response["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("no send tool")
        );
        assert!(
            session
                .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none()
        );
    }

    fn call(session: &mut Session, name: &str, arguments: Value) -> Value {
        rpc(
            session,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments }
            }),
        )
    }

    fn tool_text(response: &Value) -> String {
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn tool_json(response: &Value) -> Value {
        assert_eq!(response["result"]["isError"], false, "{response}");
        serde_json::from_str(&tool_text(response)).unwrap()
    }

    fn tool_names(session: &mut Session) -> Vec<String> {
        let response = rpc(
            session,
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
        );
        response["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn disabled_agent_has_no_tools() {
        let (mut session, _) = session_with(AgentGrant::DISABLED, "bot");
        let early = call(&mut session, "search", json!({ "query": "invoice" }));
        assert_eq!(early["error"]["message"], "server not initialized");
        init(&mut session);
        assert!(tool_names(&mut session).is_empty());
        let response = call(&mut session, "search", json!({ "query": "invoice" }));
        assert_eq!(response["result"]["isError"], true);
        assert!(tool_text(&response).contains("disabled"));
    }

    #[test]
    fn scopes_are_separate_and_send_is_unknown() {
        let (mut session, thread) = session_with(AgentGrant::enabled(true, false, false), "bot");
        init(&mut session);
        assert_eq!(
            tool_names(&mut session),
            ["search", "read_thread", "list_labels"]
        );
        let hits = tool_json(&call(&mut session, "search", json!({ "query": "invoice" })));
        assert_eq!(hits["hits"][0]["thread_id"], thread.as_str());
        let secret = tool_json(&call(
            &mut session,
            "search",
            json!({ "query": "hidden label body" }),
        ));
        assert!(secret["hits"].as_array().unwrap().is_empty());
        let read = tool_json(&call(
            &mut session,
            "read_thread",
            json!({ "thread_id": thread.as_str() }),
        ));
        assert_eq!(read["messages"][0]["body"], "please pay the invoice");
        let labels = tool_json(&call(&mut session, "list_labels", json!({})));
        assert_eq!(labels["labels"][0], "billing");
        let archived = call(
            &mut session,
            "archive",
            json!({ "thread_id": thread.as_str() }),
        );
        assert_eq!(archived["result"]["isError"], true);
        assert!(tool_text(&archived).contains("lacks the triage scope"));
        let drafted = call(
            &mut session,
            "create_draft",
            json!({ "to": ["a@x"], "body": "nope" }),
        );
        assert!(tool_text(&drafted).contains("lacks the draft scope"));
        for name in ["send", "confirm_send", "submit_draft"] {
            let response = call(&mut session, name, json!({}));
            assert_eq!(response["error"]["message"], "unknown tool", "{name}");
            assert!(response.get("result").is_none());
        }
    }

    #[test]
    fn triage_and_drafts_round_trip_without_sending() {
        let (mut session, thread) = session_with(AgentGrant::enabled(true, true, true), "bot");
        init(&mut session);
        let names = tool_names(&mut session);
        assert!(names.iter().all(|name| !name.contains("send")));
        assert!(names.contains(&"undo".to_string()));
        let labeled = tool_json(&call(
            &mut session,
            "label",
            json!({ "thread_id": thread.as_str(), "label": "news", "mode": "add" }),
        ));
        assert_eq!(labeled["changed"], true);
        let archived = tool_json(&call(
            &mut session,
            "archive",
            json!({ "thread_id": thread.as_str() }),
        ));
        assert_eq!(archived["kind"], "archive");
        tool_json(&call(
            &mut session,
            "mark_read",
            json!({ "thread_id": thread.as_str() }),
        ));
        tool_json(&call(
            &mut session,
            "snooze",
            json!({ "thread_id": thread.as_str(), "until_unix": 1_893_456_000 }),
        ));
        let read = tool_json(&call(
            &mut session,
            "read_thread",
            json!({ "thread_id": thread.as_str() }),
        ));
        assert_eq!(read["messages"][0]["archived"], true);
        assert_eq!(read["messages"][0]["unread"], false);
        assert_eq!(read["messages"][0]["snooze_until"], 1_893_456_000);
        assert!(
            !read["messages"][0]["folders"]
                .as_array()
                .unwrap()
                .iter()
                .any(|folder| folder == "INBOX")
        );
        assert_eq!(
            tool_json(&call(&mut session, "undo", json!({})))["kind"],
            "snooze"
        );
        assert_eq!(
            tool_json(&call(&mut session, "undo", json!({})))["kind"],
            "mark_read"
        );
        assert_eq!(
            tool_json(&call(&mut session, "undo", json!({})))["kind"],
            "archive"
        );
        assert_eq!(
            tool_json(&call(&mut session, "undo", json!({})))["kind"],
            "label"
        );
        let read = tool_json(&call(
            &mut session,
            "read_thread",
            json!({ "thread_id": thread.as_str() }),
        ));
        assert_eq!(read["messages"][0]["archived"], false);
        assert_eq!(read["messages"][0]["unread"], true);
        assert!(
            read["messages"][0]["folders"]
                .as_array()
                .unwrap()
                .iter()
                .any(|folder| folder == "INBOX")
        );
        let before = session
            .store
            .thread_messages(&session.account_id, &thread)
            .unwrap()
            .len();
        let draft = tool_json(&call(
            &mut session,
            "create_draft",
            json!({
                "to": ["pat@x"],
                "subject": "Re: Invoice",
                "body": "unique draft body not in the mailbox",
                "thread_id": thread.as_str(),
                "in_reply_to": "root@x"
            }),
        ));
        assert_eq!(draft["status"], "review");
        let listed = tool_json(&call(&mut session, "list_drafts", json!({})));
        assert_eq!(
            listed["drafts"][0]["body"],
            "unique draft body not in the mailbox"
        );
        assert!(
            session
                .store
                .search(&session.account_id, "unique draft body not in the mailbox")
                .unwrap()
                .hits
                .is_empty()
        );
        assert_eq!(
            session
                .store
                .thread_messages(&session.account_id, &thread)
                .unwrap()
                .len(),
            before
        );
        assert_eq!(
            tool_json(&call(&mut session, "undo", json!({})))["kind"],
            "draft"
        );
        let listed = tool_json(&call(&mut session, "list_drafts", json!({})));
        assert!(listed["drafts"].as_array().unwrap().is_empty());
        let send = call(&mut session, "send", json!({ "to": ["a@x"] }));
        assert_eq!(send["error"]["message"], "unknown tool");
    }

    #[test]
    fn draft_scope_cannot_triage_and_undo_needs_the_action_scope() {
        let (mut session, thread) = session_with(AgentGrant::enabled(false, false, true), "bot");
        init(&mut session);
        let response = call(&mut session, "search", json!({ "query": "invoice" }));
        assert!(tool_text(&response).contains("lacks the read scope"));
        let draft = tool_json(&call(
            &mut session,
            "create_draft",
            json!({ "to": ["pat@x"], "body": "review me" }),
        ));
        assert_eq!(draft["status"], "review");
        session
            .store
            .agent_archive_thread(&session.account_id, &thread, "agent:bot")
            .unwrap();
        let undo = call(&mut session, "undo", json!({}));
        assert!(tool_text(&undo).contains("lacks the triage scope"));
        let messages = session
            .store
            .thread_messages(&session.account_id, &thread)
            .unwrap();
        assert!(messages[0].archived);
        assert_eq!(
            session
                .store
                .list_review_drafts(&session.account_id)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn other_agent_stays_disabled() {
        let store = Store::open_in_memory().unwrap();
        let acct = account("a1");
        store.upsert_account(&acct).unwrap();
        store
            .upsert_message(&message("a1", "root@x", "Hello", "body", ""))
            .unwrap();
        store
            .set_agent_grant(&acct.id, "bot", AgentGrant::enabled(true, true, true))
            .unwrap();
        let mut session = Session::new(store, acct.id, "other").unwrap();
        init(&mut session);
        assert!(tool_names(&mut session).is_empty());
    }

    #[test]
    fn serve_frames_one_json_line_per_response() {
        let (mut session, _) = session_with(AgentGrant::enabled(true, false, false), "bot");
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            "\n\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":"abc","method":"tools/list"}"#,
            "\n",
        );
        let mut output = Vec::new();
        session
            .serve(std::io::Cursor::new(input.as_bytes()), &mut output)
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let init: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        let list: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(list["id"], "abc");
        assert!(
            list["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| !tool["name"].as_str().unwrap().contains("send"))
        );
    }

    #[test]
    fn parse_error_and_unknown_method() {
        let (mut session, _) = session_with(AgentGrant::DISABLED, "bot");
        let parsed = session.handle_line("not json").unwrap();
        let parsed: Value = serde_json::from_str(&parsed).unwrap();
        assert_eq!(parsed["error"]["code"], -32700);
        init(&mut session);
        let response = rpc(
            &mut session,
            json!({ "jsonrpc": "2.0", "id": 9, "method": "resources/list" }),
        );
        assert_eq!(response["error"]["code"], -32601);
    }
}
