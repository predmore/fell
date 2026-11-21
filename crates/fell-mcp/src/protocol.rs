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
        // Load the search model and vectors on their own thread now, so the
        // agent's first hybrid search does not wait for them.
        store.warm_search();
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
            .filter(|tool| !crate::tools::transmits(tool.name))
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
            "Fell agent `{}` on account `{}`. This server speaks stdio only. Tools search and read the local mailbox, list folders, threads and labels, triage (label, move, archive, star, mark read or unread, snooze, unsnooze), and save drafts for a person to review in Fell. Triage tools stay off unless that scope is granted. Mailbox changes are rate-limited. request_send only opens the compose editor. There is no send tool that transmits. Mail leaves the machine only from the Fell window. {} ",
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
            list_id: None,
            reply_to: Some(Vec::new()),
            attachment_names: Vec::new(),
            parts: None,
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
            let name = tool["name"].as_str().unwrap();
            assert!(name == "request_send" || !name.contains("send"), "{name}");
        }
    }

    /// Test model: one dimension for money words, one for everything else
    /// it knows. Nothing to download.
    struct MoneyWords;

    impl fell_core::BodyEmbedder for MoneyWords {
        fn id(&self) -> fell_core::EmbedderId {
            fell_core::EmbedderId {
                model: "test-money",
                version: "1",
            }
        }

        fn dims(&self) -> usize {
            2
        }

        fn embed(&self, body: &str) -> Result<Vec<f32>, fell_core::embed::ModelError> {
            let mut values = vec![0.0f32; 2];
            for word in body.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
                match word {
                    "pay" | "invoice" | "owed" | "money" | "bill" => values[0] += 1.0,
                    "label" | "hidden" | "garden" => values[1] += 1.0,
                    _ => {}
                }
            }
            let norm = values.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm > 0.0 {
                values.iter_mut().for_each(|v| *v /= norm);
            }
            Ok(values)
        }
    }

    #[test]
    fn search_mode_hybrid_finds_mail_close_in_meaning() {
        let (mut session, thread) = session_with(AgentGrant::enabled(true, false, false), "bot");
        session
            .store
            .upsert_message(&message(
                "a1",
                "garden@x",
                "Weekend",
                "the garden needs work",
                "home",
            ))
            .unwrap();
        session.store.testing_index_embeddings(&MoneyWords).unwrap();
        session
            .store
            .testing_search_embedder(Box::new(MoneyWords))
            .unwrap();
        init(&mut session);

        // "money owed" shares no word with the invoice mail.
        let hits = tool_json(&call(
            &mut session,
            "search",
            json!({ "query": "money owed" }),
        ));
        assert_eq!(hits["mode"], "hybrid");
        let found = hits["hits"].as_array().unwrap();
        assert_eq!(found.len(), 1, "{hits}");
        assert_eq!(found[0]["thread_id"], thread.as_str());
        assert_eq!(found[0]["related"], true);
        assert_eq!(
            found[0]["snippet"]["untrusted_email_content"]["text"],
            "please pay the invoice"
        );
        assert!(found[0].get("body").is_none());
        // The other account's mail never shows, by words or by meaning.
        let secret = tool_json(&call(
            &mut session,
            "search",
            json!({ "query": "hidden label" }),
        ));
        assert!(
            secret["hits"]
                .as_array()
                .unwrap()
                .iter()
                .all(|hit| hit["thread_id"] != "secret@x"),
            "{secret}"
        );
        // A word match is not related.
        let words = tool_json(&call(
            &mut session,
            "search",
            json!({ "query": "invoice", "mode": "hybrid" }),
        ));
        assert_eq!(words["hits"][0]["related"], false);
        assert!(words["did_you_mean"].is_null(), "{words}");
        // A misspelling: the hits are for the correction, and say so.
        let typo = tool_json(&call(
            &mut session,
            "search",
            json!({ "query": "invioce", "mode": "hybrid" }),
        ));
        assert_eq!(typo["mode"], "hybrid");
        assert_eq!(typo["did_you_mean"], "invoice", "{typo}");
        assert_eq!(typo["hits"][0]["thread_id"], thread.as_str());
        assert_eq!(typo["hits"][0]["related"], false);
        // keyword mode is word matching only.
        let keyword = tool_json(&call(
            &mut session,
            "search",
            json!({ "query": "money owed", "mode": "keyword" }),
        ));
        assert_eq!(keyword["mode"], "keyword");
        assert!(keyword["hits"].as_array().unwrap().is_empty());
        let bad = call(
            &mut session,
            "search",
            json!({ "query": "money", "mode": "vector" }),
        );
        assert_eq!(bad["result"]["isError"], true);
        assert!(tool_text(&bad).contains("mode must be keyword or hybrid"));
        // A cursor near usize::MAX is refused, not wrapped into an earlier page.
        let huge = call(
            &mut session,
            "search",
            json!({ "query": "money owed", "mode": "hybrid", "cursor": usize::MAX.to_string() }),
        );
        assert_eq!(huge["result"]["isError"], true, "{huge}");
        assert!(tool_text(&huge).contains("cursor is not a page token"));
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
            [
                "search",
                "read_thread",
                "list_folders",
                "list_threads",
                "list_labels"
            ]
        );
        let hits = tool_json(&call(&mut session, "search", json!({ "query": "invoice" })));
        // No local model: the default is keyword search, as before.
        assert_eq!(hits["mode"], "keyword");
        assert!(hits["hits"][0].get("related").is_none());
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
        assert!(
            names
                .iter()
                .all(|name| name == "request_send" || !name.contains("send"))
        );
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
            .set_agent_grant(
                &session.account_id,
                "bot",
                AgentGrant::enabled(false, true, true),
            )
            .unwrap();
        session
            .store
            .agent_archive_thread(&session.account_id, &thread, "agent:bot")
            .unwrap();
        session
            .store
            .set_agent_grant(
                &session.account_id,
                "bot",
                AgentGrant::enabled(false, false, true),
            )
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
                .all(|tool| {
                    let name = tool["name"].as_str().unwrap();
                    name == "request_send" || !name.contains("send")
                })
        );
    }

    #[test]
    fn triage_tools_stay_off_without_the_scope_and_mutations_are_limited() {
        let (mut session, _thread) = session_with(AgentGrant::enabled(true, false, false), "bot");
        init(&mut session);
        let (mut session, thread) = (session, _thread);
        for name in TRIAGE_TOOLS {
            assert!(
                !tool_names(&mut session).iter().any(|tool| tool == name),
                "{name}"
            );
            let denied = call(
                &mut session,
                name,
                json!({ "thread_id": thread.as_str(), "to": "Work", "label": "x", "mode": "add", "until_unix": 1 }),
            );
            assert_eq!(denied["result"]["isError"], true, "{name}");
            assert!(
                tool_text(&denied).contains("lacks the triage scope"),
                "{name}: {denied}"
            );
        }
        assert!(
            session
                .store
                .list_actions(&session.account_id)
                .unwrap()
                .is_empty()
        );
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
    fn agent_labels_outside_server_name_rules_stay_local() {
        for (backend_kind, names) in [
            (BackendKind::Imap, &["a\\Deleted", "x \\Seen", "foo)"][..]),
            (BackendKind::Bridge, &["Labels/../INBOX"][..]),
        ] {
            let (mut session, thread) = session_with(AgentGrant::enabled(true, true, false), "bot");
            let account = Account {
                backend_kind,
                ..account("a1")
            };
            session.store.upsert_account(&account).unwrap();
            session.store.set_write_back(&account.id, true).unwrap();
            init(&mut session);
            for name in names {
                let result = tool_json(&call(
                    &mut session,
                    "label",
                    json!({ "thread_id": thread.as_str(), "label": name, "mode": "add" }),
                ));
                assert_eq!(result["changed"], true);
            }
            let labels = session.store.list_labels(&account.id).unwrap();
            assert!(
                names
                    .iter()
                    .all(|name| labels.iter().any(|label| label == *name))
            );
            assert!(session.store.outbox(&account.id).unwrap().is_empty());
        }
    }

    #[test]
    fn read_thread_tells_a_body_not_downloaded_from_an_empty_one() {
        let (mut session, thread) = session_with(AgentGrant::enabled(true, false, false), "bot");
        // Rows a first sync listed from their headers: one still waiting
        // for its body, one whose body is too large to keep.
        let header_row = |id: &str, uid: u32| {
            let mut row = message("a1", id, "Re: Invoice March", "", "");
            row.headers.in_reply_to = Some(MessageId::new("root@x"));
            row.headers.references = vec![MessageId::new("root@x")];
            row.headers.date_unix = 30 + i64::from(uid);
            row.thread_id = thread.clone();
            row.remote_id = RemoteId(format!("imap:3:{uid}:INBOX"));
            row.body_text = None;
            row
        };
        session
            .store
            .upsert_message(&header_row("waiting@x", 1))
            .unwrap();
        session
            .store
            .upsert_message(&header_row("huge@x", 2))
            .unwrap();
        session
            .store
            .apply_delta(
                &session.account_id,
                &fell_core::MailboxDelta {
                    too_large: vec![MessageId::new("huge@x")],
                    ..fell_core::MailboxDelta::default()
                },
            )
            .unwrap();
        init(&mut session);
        let read = tool_json(&call(
            &mut session,
            "read_thread",
            json!({ "thread_id": thread.as_str() }),
        ));
        let messages = read["messages"].as_array().unwrap();
        let by_id = |id: &str| {
            messages
                .iter()
                .find(|message| message["message_id"] == id)
                .unwrap_or_else(|| panic!("{id} in {read}"))
                .clone()
        };
        let root = by_id("root@x");
        assert_eq!(root["body_pending"], false);
        assert_eq!(root["body_too_large"], false);
        assert!(root["body"]["untrusted_email_content"]["text"].is_string());
        let waiting = by_id("waiting@x");
        assert_eq!(waiting["body_pending"], true);
        assert_eq!(waiting["body_too_large"], false);
        assert!(waiting["body"].is_null());
        let huge = by_id("huge@x");
        assert_eq!(huge["body_pending"], false);
        assert_eq!(huge["body_too_large"], true);
        assert!(huge["body"].is_null());
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
            .set_agent_grant(
                &session.account_id,
                "other",
                AgentGrant::enabled(false, false, true),
            )
            .unwrap();
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
    fn request_send_and_draft_edits_never_transmit() {
        // The draft tools fall back to the default mailbox path, so on a
        // machine with real mail this test would open it.
        if fell_core::test_support::skip_unless_available(
            "request_send_and_draft_edits_never_transmit",
            fell_core::test_support::real_mailbox_present("r1"),
        ) {
            return;
        }
        let (mut session, _) = session_with(AgentGrant::enabled(true, true, true), "bot");
        init(&mut session);
        let created = tool_json(&call(
            &mut session,
            "create_draft",
            json!({
                "to": ["ada@example.com"],
                "subject": "Hi",
                "body": "alpha paragraph"
            }),
        ));
        let id = created["compose_id"].as_str().unwrap();
        let edited = tool_json(&call(
            &mut session,
            "edit_draft",
            json!({
                "draft_id": id,
                "base_revision": 1,
                "edits": [{"old": "alpha", "new": "beta"}],
                "send": true
            }),
        ));
        assert_eq!(edited["transmitted"], false);
        assert!(edited["draft_md"].as_str().unwrap().contains("beta"));
        let requested = tool_json(&call(
            &mut session,
            "request_send",
            json!({ "draft_id": id, "send": true, "confirm": true }),
        ));
        assert_eq!(requested["transmitted"], false);
        assert_eq!(requested["opened"], false);
        let encoded = serde_json::to_string(&requested).unwrap();
        assert!(!encoded.contains("fixture-sent"));
        let context = tool_json(&call(
            &mut session,
            "get_compose_context",
            json!({ "draft_id": id }),
        ));
        let cwd = context["cwd"].as_str().unwrap();
        assert!(!std::path::Path::new(cwd).join("thread").exists());
        assert!(std::path::Path::new(cwd).join("AGENTS.md").is_file());

        fell_core::compose::DraftHub::new()
            .discard(&session.store, &session.account_id, &[id.to_string()])
            .unwrap();
        let listed = tool_json(&call(&mut session, "list_drafts", json!({})));
        assert_eq!(listed["drafts"], json!([]));
        for tool in ["get_draft", "get_compose_context", "request_send"] {
            let denied = call(&mut session, tool, json!({ "draft_id": id }));
            assert_eq!(denied["result"]["isError"], true, "{denied}");
        }
        session.store.undo(&session.account_id).unwrap();
        let listed = tool_json(&call(&mut session, "list_drafts", json!({})));
        assert_eq!(listed["drafts"].as_array().unwrap().len(), 1);
        let context = tool_json(&call(
            &mut session,
            "get_compose_context",
            json!({ "draft_id": id }),
        ));
        assert_eq!(context["transmitted"], false);
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

    const TRIAGE_TOOLS: [&str; 8] = [
        "label",
        "move",
        "archive",
        "star",
        "mark_read",
        "mark_unread",
        "snooze",
        "unsnooze",
    ];

    /// The seeded Invoice thread (date 30, INBOX and Work, unread, billing)
    /// plus four more threads. Returns ids newest first.
    fn triage_inbox(session: &Session) -> Vec<ThreadId> {
        let mut ids = Vec::new();
        for (id, date, unread, folders, label) in [
            ("newest@x", 90, true, &["INBOX"][..], "billing"),
            ("read@x", 70, false, &["INBOX"][..], ""),
            ("sent@x", 60, false, &["Sent"][..], ""),
            ("older@x", 50, true, &["INBOX"][..], ""),
        ] {
            let mut next = message("a1", id, id, &format!("body of {id}"), label);
            next.headers.date_unix = date;
            next.unread = unread;
            next.folders = folders.iter().map(|name| (*name).to_string()).collect();
            ids.push(session.store.upsert_message(&next).unwrap());
        }
        ids
    }

    #[test]
    fn list_folders_and_list_threads_page_newest_first() {
        let (mut session, invoice) = session_with(AgentGrant::enabled(true, false, false), "bot");
        let ids = triage_inbox(&session);
        init(&mut session);

        let folders = tool_json(&call(&mut session, "list_folders", json!({})));
        let folders = folders["folders"].as_array().unwrap();
        assert_eq!(
            folders[0],
            json!({ "name": "INBOX", "role": "inbox", "unread_threads": 3 })
        );
        let work = folders.iter().find(|row| row["name"] == "Work").unwrap();
        assert_eq!(work["role"], Value::Null);
        assert_eq!(work["unread_threads"], 1);
        let sent = folders.iter().find(|row| row["name"] == "Sent").unwrap();
        assert_eq!(sent["role"], "sent");
        assert_eq!(sent["unread_threads"], 0);

        // Inbox by default, newest first, two at a time.
        let first = tool_json(&call(&mut session, "list_threads", json!({ "limit": 2 })));
        assert_eq!(first["folder"], "INBOX");
        assert_eq!(first["unread_threads"], 3);
        let rows = first["threads"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["thread_id"], ids[0].as_str());
        assert_eq!(rows[1]["thread_id"], ids[1].as_str());
        assert_eq!(rows[0]["sender_address"], "ada@x");
        assert_eq!(rows[0]["date_unix"], 90);
        assert_eq!(rows[0]["message_count"], 1);
        assert_eq!(rows[0]["unread"], true);
        assert_eq!(rows[0]["starred"], false);
        assert_eq!(rows[0]["labels"], json!(["billing"]));
        assert_eq!(
            rows[0]["subject"]["untrusted_email_content"]["text"],
            "newest@x"
        );
        assert_eq!(
            rows[0]["snippet"]["untrusted_email_content"]["text"],
            "body of newest@x"
        );
        assert!(rows[0].get("body").is_none());
        let cursor = first["next_cursor"].as_str().unwrap();
        let second = tool_json(&call(
            &mut session,
            "list_threads",
            json!({ "limit": 2, "cursor": cursor }),
        ));
        let rows = second["threads"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["thread_id"], ids[3].as_str());
        assert_eq!(rows[1]["thread_id"], invoice.as_str());
        assert_eq!(second["next_cursor"], Value::Null);

        let unread = tool_json(&call(
            &mut session,
            "list_threads",
            json!({ "unread_only": true, "since": 40 }),
        ));
        let unread: Vec<_> = unread["threads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["thread_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(unread, [ids[0].as_str(), ids[3].as_str()]);

        let sent = tool_json(&call(
            &mut session,
            "list_threads",
            json!({ "folder": "sent" }),
        ));
        assert_eq!(sent["threads"][0]["thread_id"], ids[2].as_str());

        let billing = tool_json(&call(
            &mut session,
            "list_threads",
            json!({ "label": "billing" }),
        ));
        assert_eq!(billing["label"], "billing");
        assert!(billing.get("unread_threads").is_none());
        let billing: Vec<_> = billing["threads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["thread_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(billing, [ids[0].as_str(), invoice.as_str()]);

        for (args, error) in [
            (json!({ "folder": "INBOX", "label": "billing" }), "not both"),
            (json!({ "limit": 0 }), "limit must be"),
            (json!({ "cursor": "nonsense" }), "cursor must be"),
            (json!({ "unread_only": "yes" }), "must be a boolean"),
        ] {
            let response = call(&mut session, "list_threads", args);
            assert_eq!(response["result"]["isError"], true, "{response}");
            assert!(tool_text(&response).contains(error), "{response}");
        }
        let capped = tool_json(&call(&mut session, "list_threads", json!({ "limit": 500 })));
        assert_eq!(capped["threads"].as_array().unwrap().len(), 4);

        let (mut drafter, _) = session_with(AgentGrant::enabled(false, false, true), "bot");
        init(&mut drafter);
        for name in ["list_folders", "list_threads"] {
            let denied = call(&mut drafter, name, json!({}));
            assert!(
                tool_text(&denied).contains("lacks the read scope"),
                "{name}"
            );
        }
    }

    #[test]
    fn move_star_mark_unread_and_unsnooze_are_logged_and_undone() {
        let (mut session, thread) = session_with(AgentGrant::enabled(true, true, false), "bot");
        init(&mut session);
        let read_first = |session: &mut Session| {
            tool_json(&call(
                session,
                "read_thread",
                json!({ "thread_id": thread.as_str() }),
            ))["messages"][0]
                .clone()
        };
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
        let moved = tool_json(&call(
            &mut session,
            "move",
            json!({ "thread_id": thread.as_str(), "to": "work" }),
        ));
        assert_eq!(moved["kind"], "move");
        assert_eq!(moved["summary"], "moved thread from INBOX to Work");
        let starred = tool_json(&call(
            &mut session,
            "star",
            json!({ "thread_id": thread.as_str() }),
        ));
        assert_eq!(starred["kind"], "star");
        let again = tool_json(&call(
            &mut session,
            "star",
            json!({ "thread_id": thread.as_str(), "starred": true }),
        ));
        assert_eq!(again["changed"], false);
        let unread = tool_json(&call(
            &mut session,
            "mark_unread",
            json!({ "thread_id": thread.as_str() }),
        ));
        assert_eq!(unread["kind"], "mark_unread");
        let unsnoozed = tool_json(&call(
            &mut session,
            "unsnooze",
            json!({ "thread_id": thread.as_str() }),
        ));
        assert_eq!(unsnoozed["kind"], "unsnooze");

        let now = read_first(&mut session);
        assert_eq!(now["folders"], json!(["Work"]));
        assert_eq!(now["starred"], true);
        assert_eq!(now["unread"], true);
        assert_eq!(now["snooze_until"], Value::Null);
        let inbox = tool_json(&call(&mut session, "list_threads", json!({})));
        assert!(inbox["threads"].as_array().unwrap().is_empty());

        for kind in ["unsnooze", "mark_unread", "star", "move"] {
            assert_eq!(
                tool_json(&call(&mut session, "undo", json!({})))["kind"],
                kind
            );
        }
        let back = read_first(&mut session);
        assert_eq!(back["folders"], json!(["INBOX", "Work"]));
        assert_eq!(back["starred"], false);
        assert_eq!(back["unread"], false);
        assert_eq!(back["snooze_until"], 1_893_456_000);
        let unstar = tool_json(&call(
            &mut session,
            "star",
            json!({ "thread_id": thread.as_str(), "starred": false }),
        ));
        assert_eq!(unstar["changed"], false);
    }

    #[test]
    fn move_refuses_trash_spam_and_folders_the_mailbox_lacks() {
        let (mut session, thread) = session_with(AgentGrant::enabled(true, true, false), "bot");
        let mut junk = message("a1", "junk@x", "Junk", "junk body", "");
        junk.folders = vec!["Junk".into()];
        let junk = session.store.upsert_message(&junk).unwrap();
        init(&mut session);
        for to in ["Trash", "deleted", "Spam", "junk", "Sent", "Drafts"] {
            let refused = call(
                &mut session,
                "move",
                json!({ "thread_id": thread.as_str(), "to": to }),
            );
            assert_eq!(refused["result"]["isError"], true, "{to}");
            assert!(
                tool_text(&refused).contains("cannot move mail to Trash, Spam"),
                "{to}: {refused}"
            );
        }
        let unknown = call(
            &mut session,
            "move",
            json!({ "thread_id": thread.as_str(), "to": "Receipts" }),
        );
        assert!(tool_text(&unknown).contains("list_folders"), "{unknown}");
        let wrong_source = call(
            &mut session,
            "move",
            json!({ "thread_id": thread.as_str(), "from": "Sent", "to": "Work" }),
        );
        assert!(tool_text(&wrong_source).contains("no messages in Sent"));
        assert!(
            session
                .store
                .list_actions(&session.account_id)
                .unwrap()
                .is_empty()
        );
        // Archive is always a destination, as it is for the archive tool.
        let archived = tool_json(&call(
            &mut session,
            "move",
            json!({ "thread_id": thread.as_str(), "to": "Archive" }),
        ));
        assert_eq!(archived["kind"], "move");
        // Rescuing mail out of Spam is allowed.
        let rescued = tool_json(&call(
            &mut session,
            "move",
            json!({ "thread_id": junk.as_str(), "from": "Spam", "to": "INBOX" }),
        ));
        assert_eq!(rescued["summary"], "moved thread from Spam to INBOX");
    }

    /// Record `roles` as if the server's LIST reply had marked them
    /// (SPECIAL-USE), as sync does.
    fn mark_roles(session: &Session, roles: &[(fell_core::FolderRole, &str)]) {
        let folders = roles
            .iter()
            .map(|(role, name)| fell_core::RoleFolder {
                role: *role,
                name: (*name).into(),
                raw: (*name).into(),
                source: fell_core::RoleSource::Server,
            })
            .collect();
        session
            .store
            .apply_delta(
                &session.account_id,
                &fell_core::MailboxDelta {
                    roles: Some(fell_core::FolderRoles::new(folders)),
                    ..Default::default()
                },
            )
            .unwrap();
    }

    fn stored_in(session: &Session, id: &str, folders: &[&str]) -> ThreadId {
        let mut filed = message("a1", id, id, "body", "");
        filed.folders = folders.iter().map(|name| (*name).to_string()).collect();
        session.store.upsert_message(&filed).unwrap()
    }

    /// A server that names its Sent or Trash its own way still has them
    /// refused: the check reads the server's marks, not a name table.
    #[test]
    fn move_refuses_server_marked_trash_sent_spam_and_drafts() {
        use fell_core::FolderRole;
        let (mut session, thread) = session_with(AgentGrant::enabled(true, true, false), "bot");
        init(&mut session);
        for (role, folder) in [
            (FolderRole::Sent, "Sent Items"),
            (FolderRole::Trash, "Deleted Messages"),
            (FolderRole::Trash, "[Gmail]/Trash"),
            (FolderRole::Junk, "Bulk"),
            (FolderRole::Drafts, "Brouillons"),
        ] {
            stored_in(&session, &format!("{folder}@x"), &[folder]);
            mark_roles(&session, &[(FolderRole::Inbox, "INBOX"), (role, folder)]);
            let listed = tool_json(&call(&mut session, "list_folders", json!({})));
            let entry = listed["folders"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["name"] == folder)
                .unwrap_or_else(|| panic!("{folder} not listed: {listed}"));
            assert_eq!(entry["role"], role.key(), "{folder}");
            for to in [folder.to_string(), folder.to_ascii_lowercase()] {
                let refused = call(
                    &mut session,
                    "move",
                    json!({ "thread_id": thread.as_str(), "to": to }),
                );
                assert_eq!(refused["result"]["isError"], true, "{to}");
                assert!(
                    tool_text(&refused).contains("cannot move mail to Trash, Spam, Sent or Drafts"),
                    "{to}: {refused}"
                );
            }
        }
        // Gmail's All Mail and Starred are views: moving into one is
        // archive or star, and moving out would drop it from the view.
        stored_in(&session, "all@x", &["[Gmail]/All Mail", "[Gmail]/Starred"]);
        mark_roles(
            &session,
            &[
                (FolderRole::All, "[Gmail]/All Mail"),
                (FolderRole::Flagged, "[Gmail]/Starred"),
            ],
        );
        for args in [
            json!({ "thread_id": thread.as_str(), "to": "[Gmail]/All Mail" }),
            json!({ "thread_id": thread.as_str(), "to": "[Gmail]/Starred" }),
            json!({ "thread_id": thread.as_str(), "from": "[Gmail]/All Mail", "to": "Work" }),
        ] {
            let refused = call(&mut session, "move", args.clone());
            assert!(
                tool_text(&refused).contains("use archive or star"),
                "{args}: {refused}"
            );
        }
        assert!(
            session
                .store
                .list_actions(&session.account_id)
                .unwrap()
                .is_empty()
        );
    }

    /// Once the server marks its folders, a user folder that happens to be
    /// called Sent or Trash is just a folder, and mail can be moved out of
    /// Trash and Spam and back again with undo.
    #[test]
    fn move_files_into_user_folders_and_out_of_trash_and_spam() {
        use fell_core::FolderRole;
        let (mut session, thread) = session_with(AgentGrant::enabled(true, true, false), "bot");
        let trashed = stored_in(&session, "trashed@x", &["[Gmail]/Trash"]);
        let spam = stored_in(&session, "spam@x", &["[Gmail]/Spam"]);
        stored_in(&session, "label@x", &["Projects", "Trash"]);
        mark_roles(
            &session,
            &[
                (FolderRole::Inbox, "INBOX"),
                (FolderRole::Trash, "[Gmail]/Trash"),
                (FolderRole::Junk, "[Gmail]/Spam"),
            ],
        );
        init(&mut session);
        for to in ["Projects", "Trash"] {
            let moved = tool_json(&call(
                &mut session,
                "move",
                json!({ "thread_id": thread.as_str(), "from": "Work", "to": to }),
            ));
            assert_eq!(moved["kind"], "move", "{to}: {moved}");
            let undone = tool_json(&call(&mut session, "undo", json!({})));
            assert_eq!(undone["kind"], "move");
        }
        for (id, from) in [(&trashed, "[Gmail]/Trash"), (&spam, "[Gmail]/Spam")] {
            let rescued = tool_json(&call(
                &mut session,
                "move",
                json!({ "thread_id": id.as_str(), "from": from, "to": "INBOX" }),
            ));
            assert_eq!(
                rescued["summary"],
                format!("moved thread from {from} to INBOX")
            );
            let read = tool_json(&call(
                &mut session,
                "read_thread",
                json!({ "thread_id": id.as_str() }),
            ));
            assert_eq!(read["messages"][0]["folders"], json!(["INBOX"]));
            assert_eq!(
                tool_json(&call(&mut session, "undo", json!({})))["kind"],
                "move"
            );
            let back = tool_json(&call(
                &mut session,
                "read_thread",
                json!({ "thread_id": id.as_str() }),
            ));
            assert_eq!(back["messages"][0]["folders"], json!([from]));
        }
    }

    /// A message with more names than read_thread lists says how many it
    /// left out, as a cut body says it was truncated.
    #[test]
    fn read_thread_counts_attachment_names_it_leaves_out() {
        let (mut session, _) = session_with(AgentGrant::enabled(true, false, false), "bot");
        let mut many = message("a1", "many@x", "Scans", "see attached", "");
        many.has_attachment = true;
        many.attachment_names = (1..=25).map(|n| format!("scan-{n:02}.pdf")).collect();
        let many = session.store.upsert_message(&many).unwrap();
        let mut few = message("a1", "few@x", "One scan", "see attached", "");
        few.has_attachment = true;
        few.attachment_names = vec!["scan.pdf".into()];
        let few = session.store.upsert_message(&few).unwrap();
        init(&mut session);
        let read = tool_json(&call(
            &mut session,
            "read_thread",
            json!({ "thread_id": many.as_str() }),
        ));
        let message = &read["messages"][0];
        let names = message["attachments"].as_array().unwrap();
        assert_eq!(names.len(), 20);
        assert_eq!(
            names[19]["name"]["untrusted_email_content"]["text"],
            "scan-20.pdf"
        );
        assert_eq!(message["attachments_omitted"], 5);
        let read = tool_json(&call(
            &mut session,
            "read_thread",
            json!({ "thread_id": few.as_str() }),
        ));
        assert_eq!(read["messages"][0]["attachments_omitted"], 0);
    }

    #[test]
    fn read_thread_lists_attachment_names_inside_the_envelope() {
        let (mut session, _) = session_with(AgentGrant::enabled(true, false, false), "bot");
        let mut invoice = message("a1", "pdf@x", "Invoice", "see attached", "");
        invoice.has_attachment = true;
        invoice.attachment_names = vec!["invoice-march.pdf".into()];
        let thread = session.store.upsert_message(&invoice).unwrap();
        init(&mut session);
        let read = tool_json(&call(
            &mut session,
            "read_thread",
            json!({ "thread_id": thread.as_str() }),
        ));
        let message = &read["messages"][0];
        assert_eq!(message["has_attachment"], true);
        assert_eq!(
            message["attachments"],
            json!([{ "name": { "untrusted_email_content": {
                "text": "invoice-march.pdf",
                "truncated": false,
            } } }])
        );
    }

    #[test]
    fn read_thread_gives_type_and_size_from_listed_parts_never_bytes() {
        let (mut session, _) = session_with(AgentGrant::enabled(true, false, false), "bot");
        let part = |section: &str, role, mime: &str, name: &str, size| fell_core::MimePart {
            section: section.into(),
            role,
            mime_type: mime.into(),
            filename: Some(name.into()),
            size,
            encoded_size: size * 4 / 3,
            encoding: fell_core::TransferEncoding::Base64,
            content_id: None,
            disposition: fell_core::Disposition::Attachment,
            nested: false,
        };
        let mut signed = message("a1", "signed@x", "Report", "see attached", "");
        signed.has_attachment = true;
        signed.parts = Some(fell_core::MessageParts {
            plain: Some("see attached".into()),
            mime_parts: vec![
                part(
                    "2",
                    fell_core::PartRole::Attachment,
                    "application/pdf",
                    "q3.pdf",
                    48_213,
                ),
                part(
                    "3",
                    fell_core::PartRole::Signature,
                    "application/pgp-signature",
                    "signature.asc",
                    833,
                ),
            ],
            ..Default::default()
        });
        let thread = session.store.upsert_message(&signed).unwrap();
        init(&mut session);
        let read = tool_json(&call(
            &mut session,
            "read_thread",
            json!({ "thread_id": thread.as_str() }),
        ));
        let message = &read["messages"][0];
        assert_eq!(
            message["attachments"],
            json!([{
                "name": { "untrusted_email_content": { "text": "q3.pdf", "truncated": false } },
                "type": { "untrusted_email_content": { "text": "application/pdf", "truncated": false } },
                "size": 48_213,
            }])
        );
        assert_eq!(message["attachments_omitted"], 0);
    }

    /// Every tool, whatever it is passed, neither sends nor says it did.
    /// Mail leaves only from the editor's own send action.
    #[test]
    fn no_tool_transmits_whatever_it_is_given() {
        let (mut session, thread) = session_with(AgentGrant::enabled(true, true, true), "bot");
        init(&mut session);
        let listed = tool_names(&mut session);
        let names = crate::tools::all_tool_names();
        assert_eq!(listed, names);
        for name in names {
            for args in [
                json!({}),
                json!({ "thread_id": thread.as_str(), "send": true, "confirm": true, "transmit": true }),
            ] {
                let response = call(&mut session, name, args);
                assert!(response.get("error").is_none(), "{name}: {response}");
                let text = tool_text(&response);
                if let Ok(body) = serde_json::from_str::<Value>(&text) {
                    assert_ne!(body["transmitted"], true, "{name}: {text}");
                    assert!(body.get("sent").is_none(), "{name}: {text}");
                }
            }
        }
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
