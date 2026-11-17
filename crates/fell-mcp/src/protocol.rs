//! Newline-delimited JSON-RPC for MCP stdio.
//!
//! Tool failures come back as a tool result with `isError`. A name that is
//! not in the registry, including anything that would send mail, is a
//! JSON-RPC error so it never looks like a tool Fell supports.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use fell_core::{AccountId, Store};
use serde_json::{Value, json};

use crate::tools;

const PROTOCOL_VERSION: &str = "2024-11-05";
const SUPPORTED_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

pub struct Session {
    pub(crate) store: Store,
    pub(crate) account_id: AccountId,
    pub(crate) agent_id: String,
    pub(crate) mutations: MutationBudget,
    /// Aliases for headers that are not already safe tokens.
    ids: RefCell<ShownIds>,
    initialized: bool,
}

/// Shown id to stored header. A shown id is never reused for a second row.
struct ShownIds {
    by_shown: HashMap<String, String>,
    by_raw: HashMap<String, String>,
    seed: u64,
    counter: u64,
}

impl ShownIds {
    fn new() -> Self {
        Self {
            by_shown: HashMap::new(),
            by_raw: HashMap::new(),
            seed: std::collections::hash_map::RandomState::new().hash_one(0xFEu64),
            counter: 0,
        }
    }
}

/// Caps mailbox changes from one agent. The default is 30 per minute.
pub(crate) struct MutationBudget {
    window: Duration,
    limit: u32,
    started: Instant,
    used: u32,
}

impl MutationBudget {
    fn production() -> Self {
        Self {
            window: Duration::from_secs(60),
            limit: 30,
            started: Instant::now(),
            used: 0,
        }
    }

    pub(crate) fn allow(&mut self) -> Result<(), String> {
        let now = Instant::now();
        if now.duration_since(self.started) >= self.window {
            self.started = now;
            self.used = 0;
        }
        if self.used >= self.limit {
            return Err(format!(
                "too many mailbox changes; at most {} every {} seconds",
                self.limit,
                self.window.as_secs()
            ));
        }
        self.used += 1;
        Ok(())
    }
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
            mutations: MutationBudget::production(),
            ids: RefCell::new(ShownIds::new()),
            initialized: false,
        })
    }

    pub(crate) fn show_id(&self, raw: &str) -> String {
        if let Some(shown) = self.ids.borrow().by_raw.get(raw).cloned() {
            return shown;
        }
        if let Some(verbatim) = crate::untrusted::verbatim_id(raw)
            && !self.ids.borrow().by_shown.contains_key(verbatim)
        {
            return verbatim.to_string();
        }
        loop {
            let shown = self.allocate_alias(raw);
            let mut ids = self.ids.borrow_mut();
            if let Some(existing) = ids.by_raw.get(raw).cloned() {
                return existing;
            }
            if ids.by_shown.contains_key(&shown) {
                continue;
            }
            ids.by_shown.insert(shown.clone(), raw.to_string());
            ids.by_raw.insert(raw.to_string(), shown.clone());
            return shown;
        }
    }

    pub(crate) fn resolve_id(&self, given: &str) -> String {
        self.ids
            .borrow()
            .by_shown
            .get(given)
            .cloned()
            .unwrap_or_else(|| given.to_string())
    }

    /// A fresh id for this session. It is not a stored token and not another row's alias.
    fn allocate_alias(&self, raw: &str) -> String {
        let mut failures = 0;
        loop {
            let candidate = {
                let mut ids = self.ids.borrow_mut();
                ids.counter = ids.counter.wrapping_add(1);
                format!("fellid:{:016x}{:016x}", ids.seed, ids.counter)
            };
            if candidate == raw {
                continue;
            }
            let taken = {
                let ids = self.ids.borrow();
                ids.by_shown.contains_key(&candidate) || ids.by_raw.contains_key(&candidate)
            };
            if taken {
                continue;
            }
            match self.store.id_is_stored(&self.account_id, &candidate) {
                Ok(false) => return candidate,
                Ok(true) => continue,
                Err(_) => {
                    failures += 1;
                    if failures >= 4 {
                        return candidate;
                    }
                }
            }
        }
    }

    #[cfg(test)]
    fn set_alias_seed(&self, seed: u64) {
        let mut ids = self.ids.borrow_mut();
        ids.seed = seed;
        ids.counter = 0;
    }

    #[cfg(test)]
    pub(crate) fn set_mutation_limit(&mut self, limit: u32) {
        self.mutations.limit = limit;
        self.mutations.used = 0;
        self.mutations.started = Instant::now();
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
                    "description": format!("{} {}", tool.description, crate::untrusted::UNTRUSTED_NOTE),
                    "inputSchema": tool.schema,
                })
            })
            .collect::<Vec<_>>();
        rpc_result(id, json!({ "tools": tools }))
    }

    fn tools_call(&mut self, id: &Value, params: &Value) -> String {
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
            "Fell agent `{}` on account `{}`. This server speaks stdio only. Tools search and read the local mailbox, list labels, triage (label, archive, mark read, snooze), and save drafts for a person to review in Fell. Triage tools stay off unless that scope is granted. Mailbox changes are rate-limited. There is no send tool. Mail leaves the machine only from the Fell window. {} ",
            self.agent_id,
            self.account_id.as_str(),
            crate::untrusted::UNTRUSTED_NOTE
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
            starred: false,
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
    fn every_tool_description_says_mail_is_untrusted() {
        let (mut session, _) = session_with(AgentGrant::enabled(true, true, true), "bot");
        init(&mut session);
        let response = rpc(
            &mut session,
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
        );
        let tools = response["result"]["tools"].as_array().unwrap();
        assert!(tools.len() >= 8, "{tools:?}");
        for tool in tools {
            let description = tool["description"].as_str().unwrap();
            assert!(
                description.contains(crate::untrusted::UNTRUSTED_NOTE),
                "{}",
                tool["name"]
            );
            assert!(!tool["name"].as_str().unwrap().contains("send"));
        }
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
        assert_eq!(hits["hits"][0]["sender_address"], "ada@x");
        assert_eq!(hits["hits"][0]["date_unix"], 30);
        assert!(hits["hits"][0].get("subject").is_none());
        assert!(hits["hits"][0].get("body").is_none());
        assert_eq!(
            hits["hits"][0]["snippet"]["untrusted_email_content"]["text"],
            "please pay the invoice"
        );
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
        assert_eq!(
            read["messages"][0]["body"]["untrusted_email_content"]["text"],
            "please pay the invoice"
        );
        assert_eq!(read["messages"][0]["sender_address"], "ada@x");
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
            listed["drafts"][0]["body"]["untrusted_email_content"]["text"],
            "unique draft body not in the mailbox"
        );
        assert_eq!(listed["drafts"][0]["bcc_redacted"], false);
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
    fn triage_tools_stay_off_without_the_scope_and_mutations_are_limited() {
        let (mut session, _thread) = session_with(AgentGrant::enabled(true, false, false), "bot");
        init(&mut session);
        for name in ["label", "archive", "mark_read", "snooze"] {
            assert!(
                !tool_names(&mut session).iter().any(|tool| tool == name),
                "{name}"
            );
        }
        let (mut session, thread) = session_with(AgentGrant::enabled(true, true, false), "bot");
        session.set_mutation_limit(2);
        init(&mut session);
        for (index, label) in ["one", "two"].into_iter().enumerate() {
            let result = tool_json(&call(
                &mut session,
                "label",
                json!({ "thread_id": thread.as_str(), "label": label, "mode": "add" }),
            ));
            assert_eq!(result["changed"], true, "{index}");
        }
        let limited = call(
            &mut session,
            "label",
            json!({ "thread_id": thread.as_str(), "label": "three", "mode": "add" }),
        );
        assert_eq!(limited["result"]["isError"], true);
        assert!(tool_text(&limited).contains("too many mailbox changes"));
        let labels = session.store.list_labels(&session.account_id).unwrap();
        assert!(labels.iter().any(|label| label == "two"));
        assert!(!labels.iter().any(|label| label == "three"));
        let found = tool_json(&call(&mut session, "search", json!({ "query": "invoice" })));
        assert_eq!(found["hits"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn read_thread_truncates_bodies_and_draft_lists_hide_other_bcc() {
        let (mut session, thread) = session_with(AgentGrant::enabled(true, false, true), "bot");
        let long = "y".repeat(50);
        let mut replacement = message("a1", "root@x", "Invoice March", &long, "billing");
        replacement.remote_id = fell_core::RemoteId("a1:root-long".into());
        session.store.upsert_message(&replacement).unwrap();
        session
            .store
            .create_review_draft(
                fell_core::ReviewDraftInput {
                    account_id: session.account_id.clone(),
                    to: vec!["pat@x".into()],
                    cc: vec![],
                    bcc: vec!["secret@x".into()],
                    subject: "other".into(),
                    body: "other body".into(),
                    in_reply_to: None,
                    thread_id: None,
                },
                "agent:other",
            )
            .unwrap();
        init(&mut session);
        let read = tool_json(&call(
            &mut session,
            "read_thread",
            json!({ "thread_id": thread.as_str(), "max_chars": 12 }),
        ));
        let text = read["messages"][0]["body"]["untrusted_email_content"]["text"]
            .as_str()
            .unwrap();
        assert_eq!(text.chars().count(), 12);
        assert_eq!(
            read["messages"][0]["body"]["untrusted_email_content"]["truncated"],
            true
        );
        let own = tool_json(&call(
            &mut session,
            "create_draft",
            json!({ "to": ["pat@x"], "bcc": ["mine@x"], "body": "mine" }),
        ));
        assert_eq!(own["status"], "review");
        let listed = tool_json(&call(&mut session, "list_drafts", json!({})));
        assert_eq!(listed["drafts"].as_array().unwrap().len(), 1);
        assert_eq!(listed["drafts"][0]["bcc"][0], "mine@x");
        let all = tool_json(&call(&mut session, "list_drafts", json!({ "all": true })));
        let drafts = all["drafts"].as_array().unwrap();
        assert_eq!(drafts.len(), 2);
        let other = drafts
            .iter()
            .find(|draft| draft["subject"]["untrusted_email_content"]["text"] == "other")
            .unwrap();
        assert_eq!(other["bcc_redacted"], true);
        assert!(other["bcc"].as_array().unwrap().is_empty());
        assert!(other["body"]["untrusted_email_content"]["text"].is_string());
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

    #[test]
    fn normalized_id_does_not_point_at_another_thread() {
        let (session, _) = session_with(AgentGrant::enabled(true, true, false), "bot");
        session.set_alias_seed(0);
        let planted = "fellid:00000000000000000000000000000001";
        session
            .store
            .upsert_message(&message("a1", planted, "Planted", "body", ""))
            .unwrap();
        let hostile = format!("root@x{}", '\u{200B}');
        let shown = session.show_id(&hostile);
        assert_eq!(shown, "fellid:00000000000000000000000000000002");
        assert_eq!(session.resolve_id("root@x"), "root@x");
        assert_eq!(session.resolve_id(&shown), hostile);
        let fullwidth: String = "root@x"
            .chars()
            .map(|ch| char::from_u32(ch as u32 + 0xFEE0).unwrap())
            .collect();
        let other = session.show_id(&fullwidth);
        assert_ne!(other, "root@x");
        assert_ne!(other, shown);
        assert_eq!(session.resolve_id("root@x"), "root@x");
        assert_eq!(session.show_id(&hostile), shown);
        let collision = session.show_id(&shown);
        assert_ne!(collision, shown);
        assert_eq!(session.resolve_id(&shown), hostile);
    }
}
