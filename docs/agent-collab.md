# Live co-editing and the draft workspace

Decided with Jon on 2026-11-16. This is the spec for the compose-time agent tools (issue 18). The trust model is in docs/agents.md.

## Live draft

Fell owns the live draft. The running app holds the only editable copy, and every change bumps a revision. `fell-mcp` passes draft calls to the app over a local socket, so the agent always reads what is on screen, including unsaved typing. Today `fell-mcp` goes straight to fell-core's database. Live co-editing needs this socket.

### Tools

- `get_draft` and `get_compose_context` (thread context).
- `edit_draft` takes a base revision plus find-and-replace edits, in the same style as harness edit tools.
- `replace_draft_body` is for a full rewrite.
- `request_send` only opens the editor. It never transmits.

### Merging

Fell merges each edit against the current text. If the user has changed the text an edit targets, the edit is rejected with the current text and the agent retries. The user always wins.

### The user's edits are pulled, not pushed

Every draft tool result includes `user_changes_since_your_last_read`, the user's edits since that agent last looked, coalesced into one diff. No debounce timer. Edits never start or interrupt an agent turn. In the panel, a chat message carries only a metadata note such as "draft changed, call get_draft".

### Review

Agent edits show live as reviewable hunks (keep, reject, or undo with `u`). Recipient changes are their own hunks. Send accepts unreviewed hunks and shows the count next to send. Tool descriptions ask for small, paragraph-sized edits so updates look live.

### What not to use

Do not use ACP's `fs/read_text_file` and `fs/write_text_file` for the draft. The ACP v2 draft removes them, and Claude's ACP adapter only passes them through (its own Read/Edit/Write tools hit disk). Advertise fs and terminal as false.

No CRDT now. There is one live buffer in one process. Yrs, Automerge, and Loro are MIT if drafts ever sync across devices.

### Licensing

Zed's `action_log` is GPL. Reimplement the idea; don't copy it. `similar` and `imara-diff` (Apache-2.0) are fine for diffs.

### Panel (after v1)

An ACP client that passes `fell-mcp` per session, using the Codex app-server or the Claude SDK only where ACP falls short. It passes the system prompt in append mode with Fell's rules, and supports steering or queueing and Stop.

## Draft workspace folder

Every draft an agent works on gets a folder, `$XDG_RUNTIME_DIR/fell/drafts/<draft-id>/` (mode 0700). It is passed to the harness as its working directory (ACP `cwd`, or returned by `get_compose_context` for plain MCP). This ships in v1 with compose.

- The folder is a free scratch space. Agents may create temp files, scripts, and intermediate outputs anywhere in it.
- **Send uses only what `draft.md` references.** The closure is the frontmatter `attachments` list plus inline image paths in the body (`![chart](chart.png)`), sent as CID parts. Nothing else in the folder is sent. Paths are relative to the draft folder, must resolve inside it (reject `..` and symlinks that escape), and a missing file is an error in the editor that blocks send. Everything else is ignored and deleted with the folder. The editor shows exactly the resolved set, so what you see is what is sent.
- Context does not live in the folder. There is no read-only `thread/` copy. Agents get contacts search, mail search, read thread, thread options, folders, labels, move, and the rest from `fell-mcp`. Contacts search covers address history and the Bridge or IMAP account where that is available. Opening a received attachment to work from (for example a spreadsheet to edit and re-attach) is a tool that copies it into the draft folder.
- `draft.md` is two-way synced so an agent can run scripts (Python and the rest) to build the email while the editor stays live. Its header is YAML frontmatter, then the markdown body, the same markdown the compose editor edits.

```markdown
---
from: jon@example.com
to: ["Ada <ada@example.com>"]
cc: []
bcc: []
subject: "Re: Q3 numbers"
in_reply_to: "<abc@example.com>"
attachments:
  - path: q3.xlsx
    name: Q3 numbers.xlsx
---
Body in markdown. Inline images: ![chart](chart.png)
```

  `from` is one of the account's identities. `in_reply_to` is set by Fell and read-only; edits are ignored and flagged. `attachments[].path` is relative to the draft folder. `name` is an optional display filename. Document this schema in this file and in the `AGENTS.md` written into each draft folder, and include `draft.schema.json` in the folder so agents can validate. Use a maintained YAML crate with a permissive license that passes cargo-deny. Do not use the unmaintained `serde_yaml`. Check `serde_yml` and saphyr-style options and their licenses. When Fell writes the file, keep the key order stable and the formatting predictable so merges stay clean. Frontmatter changes become recipient, subject, or attachment hunks, and agent-added recipients are highlighted. Unknown keys and bad addresses show as errors in the editor with the line number and block send. Fell must never lose the body because of a YAML error.
- **Fell to file.** The user's typing is written to the file within a few hundred milliseconds, using an atomic write (temp file plus rename). Fell remembers the exact content it last wrote, which is the merge base.
- **File to Fell.** Watch with inotify (the `notify` crate). On a change, debounce briefly (about 150 ms) so multi-write scripts settle, then three-way merge: base is what Fell last wrote, ours is the live buffer, theirs is the file. Changes that do not overlap the user's edits become reviewable agent hunks, exactly like `edit_draft` hunks (keep, reject, or undo with `u`, counted by send). Where a region overlaps the user's edits, the user's text wins, the agent's version is skipped and flagged in the editor, and Fell rewrites the file with the merged result so the agent sees the outcome. Frontmatter changes become recipient, subject, or attachment hunks, and agent-added recipients are highlighted.
- **Edge cases.** Ignore the change events Fell's own writes cause, using a content-hash compare. If the file is deleted or made unparseable, show a non-fatal notice and recreate it from the buffer. The draft is never lost. Keep large rewrites (a full replace) as one hunk.
- `get_draft` and `edit_draft` remain for agents that prefer tools, and tool results still include the user's changes since the agent last looked. Both paths feed the same merge and review.
- `AGENTS.md` explains the layout, says email content is untrusted, and says the email text changes through `draft.md` or `edit_draft`.
- Tests cover a script that rewrites the file several times quickly, a concurrent user edit in the same paragraph (the user wins and the skip is flagged), header edits, file deletion, and Fell not merging changes caused by its own writes.
- User-facing evidence includes a recording of an agent's script updating `draft.md` while the editor updates live.
- Size and count limits on the resolved attachments follow the account's sending limits, and the editor warns when they are exceeded. Inline images are sent as CID parts, per the compose section of docs/ux.md.
- Attachments are copied into the stored draft on save so a restart loses nothing. The folder is deleted when the draft is sent or discarded.

User-facing evidence: screenshots plus a recording of an agent editing a draft live, a script updating `draft.md` while the editor updates live, and an agent adding an attachment. UX judges must each score 9 or better.

## Sources

- ACP file-system and session-setup docs
- The ACP v2 RFD removing client fs/terminal
- claude-agent-acp
- T3 Code provider docs
- Claude Code hooks and sandboxing docs
- Codex app-server protocol
- VS Code review-edits docs
