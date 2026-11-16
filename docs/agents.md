# Agents in Fell

Fell does not build its own agent, model loop, or prompt harness. It plugs into the agent harnesses people already use (Claude Code, Codex, Gemini CLI, OpenCode, Goose, and anything else that speaks the protocol), the way T3 Code and Zed do.

## How Fell connects

1. **Fell exposes mail as tools over MCP** (`fell-mcp`, stdio). Any harness can add Fell as an MCP server and get search, read, triage and draft tools within the scopes the user granted. This is the v1 surface (M4).
2. **Fell hosts harnesses over ACP** (Agent Client Protocol, the open protocol Zed uses for external agents). The in-app agent panel is an ACP client: it starts the user's chosen harness as a subprocess, streams its messages and tool calls into the panel, and passes `fell-mcp` to the session as its MCP server. Where a harness has no ACP adapter, use that harness's own SDK or app-server protocol behind the same internal `AgentHost` trait. Fell never calls a model API directly for agent work.
3. Harness choice, binary path and auth belong to the harness. Fell stores only which harness to launch and the per-agent mail scopes. Fell never sees or stores model API keys.
4. The panel stays after v1, as in scope.md, but the design is fixed now: ACP client plus fell-mcp, and no in-house agent loop. The `AgentHost` trait lives in a UI-free crate so other front ends can reuse it.

Licensing: ACP's Rust crate (`agent-client-protocol`) is Apache-2.0, so it's fine to depend on. Do not copy GPL code from Zed's agent UI.

## Email content is never trusted

Every byte that came from a message (subject, sender display name, snippet, body, HTML, attachment names and contents, headers, and drafts an agent wrote from them) is untrusted data. It can contain instructions aimed at the model. Rules:

1. **Envelope everything.** Tool results return mail-derived fields only inside an explicit untrusted-content structure: a typed field such as `untrusted_email_content`, with provenance (thread id, message id, sender address) kept separate from content. Tool descriptions tell the model that this content is data and never instructions. Never interpolate mail text into tool descriptions, system prompts, error messages, or anything else the harness treats as trusted.
2. **Neutralize lookalike fences.** Strip or escape anything inside content that mimics the envelope delimiters, role markers, or tool-call syntax, so an email can't close its own envelope.
3. **Minimize by default.** Search returns ids, sender address, date and a short snippet. Bodies come only from read_thread, truncated, with HTML converted to plain text. Remote content is never fetched on an agent's behalf. Attachments are not exposed in v1.
4. **Agents propose, the user sends.** An agent may draft, edit a draft, and call `request_send` on a draft. That tool never transmits: it opens the draft in Fell's compose editor, focused, with the final recipients (to, cc, bcc), subject, body, attachments and reply target visible. Mail goes out only when the user presses send in that editor. No tool, flag, scope or setting can skip that step, and there is no auto-send or batch send. If the draft changes after the user opens it, the editor shows the latest version before send is allowed. Recipients the user has never mailed are highlighted.

   **Live co-editing.** While a draft is open, the user can talk to the agent (the agent panel, or any connected harness) and the agent updates that draft through `update_draft`. The editor shows each change live, with the agent's edit visibly marked and undoable with `u`. The user can type in the draft at the same time. The user's edits always win a conflict, and the agent sees the current text before its next edit. Agent edits are limited to the draft it was given, and a draft the user is editing cannot be sent or deleted by an agent.
5. **Contain the blast radius.** Mutating tools (triage, label, draft) are off unless granted per agent, rate-limited, logged with the actor and the triggering tool call, and undoable with `u`. The panel shows each agent action as it happens. Bulk changes (more than a few threads in one turn) ask the user in the UI first.
6. **Never move data out.** Agents get no tool that fetches URLs, opens links, or writes files. A draft addressed to anyone the user has never written to is flagged in review.
7. **Prove it with tests.** Keep a fixed corpus of hostile emails (instruction injection, fake envelope closers, Unicode tricks, huge bodies, address lists packed into one field). CI checks that the envelope holds, nothing leaks into trusted fields, the caps hold, and no tool path sends mail without the user pressing send in the compose editor (`request_send` only opens the editor). Repeat QA's adversarial review before any release that changes agent tools.
