# Architecture

Fell is a native Proton Mail client. The UI never talks to the network; it reads a local store that a sync layer fills from a mail backend.

```text
┌─────────────┐     ┌─────────────┐     ┌──────────────────┐
│  fell (GPUI)│     │   fell-mcp  │     │ Proton Mail      │
│  Wayland UI │     │  scoped MCP │     │ Bridge (IMAP)    │
└──────┬──────┘     └──────┬──────┘     └────────┬─────────┘
       │  types only       │  search/draft       │ later: Proton API
       ▼                   ▼                     ▼
┌─────────────────────────────────────────────────────────┐
│                      fell-core                          │
│  accounts · backend trait · sync · sqlite+fts · threads │
└─────────────────────────────────────────────────────────┘
```

## Crates

| Crate | Role |
| --- | --- |
| `fell-core` | Accounts, backend trait, sync, local SQLite store (FTS5), threading, search, send-confirmation types. No UI. |
| `fell-render` | Styled document shared by received HTML and later markdown compose. Chooses native vs web view. No GPUI. |
| `fell-confirm` | Sealed `InteractiveUi` (private field, not `Default`). Required to mint a send confirmation. The GPUI app depends on it; `fell-mcp` must not. |
| `fell` | GPUI application. Opens a Wayland window and paints one message with `fell::render::MessageBody`. Does not own the thread list, keymap, or command palette. Depends on `fell-core` for data, not the other way around. Mints send confirmations. |
| `fell-mcp` | Stdio MCP server. Search, read threads, list labels, triage, and review drafts. No send tool. Scopes stay off until enabled. |

## Backends

`MailBackend` is the seam.

1. **Bridge** — Proton Mail Bridge over local IMAP. Read-only sync uses the generic IMAP client with Bridge's usual host and port. Bridge stays a separate process. SMTP is not connected.
2. **IMAP** — any other IMAP account. Same read-only client.
3. **Proton API** — direct Proton HTTP API. Same trait, not implemented.

Sync logs in, lists mailboxes, and `EXAMINE`s each one (read-only). New mail is `UID FETCH` with `BODY.PEEK`, so the server is not asked to mark mail read. A cursor stores UIDVALIDITY, the highest UID, and HIGHESTMODSEQ when the server advertises CONDSTORE. The next sync fetches only newer UIDs, or `CHANGEDSINCE` when a modseq is known. IDLE and removal of messages that vanished on the server are later. There is no SMTP, no OAuth, and no send command.

The UI and MCP crates take `dyn MailBackend` (or a store already populated by sync). They do not import IMAP or Proton protocol code.

## Local store

SQLite is the source of truth for the UI. Sync writes messages, threads, and an FTS5 index. Reads do not block on the network. If Bridge or the API is down, the last synced mailbox still renders.

Each IMAP mailbox is one folder on the message. A message with no Message-ID is stored under a stable id from the mailbox, UIDVALIDITY, and UID. Copies with the same Message-ID stay one row; see Threading.

## Search

`Store::search` reads that store and nothing else. It does not call a backend, so results move only when a local write does.

The query language is FTS5 BM25 plus operators: `from:`, `to:`, `subject:`, `in:`, `label:`, `has:attachment`, `is:unread`, `before:`, and `after:`. `from:`, `to:`, and `subject:` are FTS column filters (`to:` covers To, Cc, and Bcc). Folder, label, attachment, unread, and date operators filter the message row. Every operator on one query applies to the same message. Hits are then grouped to one row per thread, ordered by that message's BM25 score (subject weighted above sender, recipients, and body). A query that is only operators still runs.

Misspellings use a character-trigram index of words from subject, sender, recipients, body, folders, and labels. Damerau-Levenshtein confirms a candidate. When the typed query matches nothing and the corrected query matches, the response carries those hits and `did_you_mean`. Semantic search is M5 and is not part of this path.

## Local actions

Archive, move, label, delete, and undo update this SQLite file only. There is no socket, no IMAP session, and no send.

The thread is the unit. Archive moves the folder the caller names (the folder on screen) to `Archive` on each message in the thread that has it. Move does the same to the folder the caller names. Any other folder on that row, such as `Sent`, stays. Delete moves that folder to `Trash`. Delete from `Trash` drops `Trash`, and removes the row when it was the last folder. Labels are a separate list. Each successful call stores the previous folders and labels on that account's undo stack. `Store::undo` restores the latest entry for that account and leaves other accounts as they are.

`in:` and `label:` read the columns those calls write. Folder and label words enter and leave the trigram vocabulary with the change. `apply_delta` still unions other folders and labels for the same Message-ID, and leaves out a folder archive or delete removed. Undo restores that folder and then drops the saved message copy, including the body and BCC. Outbound sync of these edits is separate.

## Threading

A thread id is assigned when a message is stored, then the account is re-resolved in memory from threading columns (not bodies):

1. If Proton sent a conversation id, that is the thread key (`proton:…`).
2. Otherwise join via `In-Reply-To` / `References` to an already-stored parent in the **same account**.
3. Otherwise start a thread from the message's own `Message-ID`.

Messages are unique on `(account_id, message_id)`. A second copy with the same id (a Sent copy, or the same message delivered to several aliases) stays one row and unions folders, labels, arrival addresses, and recipients, and keeps an attachment if either copy had one. A different Message-ID collapses only when the body fingerprint is exactly the same, the sender is the same, both dates are non-zero and within six hours, at least one copy has `List-Id`, and the Proton conversation ids are absent or equal. The fingerprint is only of non-empty body text. A missing body has no fingerprint, and subject is not an identity. More than one matching row stays as separate messages. A row that already remembers another Message-ID is not a collapse target for a later message. The merge keeps every body, parent, subject, proton id, folder, label, and address already stored, prefers the non-list subject, display name, and `In-Reply-To`, and copies a proton conversation id onto a row that lacks one. A same-id resync replaces `In-Reply-To` and `References` when the incoming copy has them. A later sync of the list copy does not wipe a body or parent the other copy filled in. The other Message-ID is remembered so a reply to it still joins the row.

After a delta with messages is written, each row's previous thread and its header-parent thread are unioned. A `proton:` id absorbs a non-proton header thread. Two different `proton:` ids are not unioned. Non-proton merges keep the earliest already-assigned thread id, so the first id handed back still lists the messages after a missing parent arrives. Cursor-only deltas skip the scan. A thread-id-only update does not rewrite FTS (the update trigger is `OF` subject, from name, from email, body, snippet, and the recipient columns). A delta whose rows belong to another account is rejected and does not advance the cursor.

Proton conversation ids are preferred so Bridge and the future API stay consistent with Proton's own threading, and two Proton conversations stay two threads. Sync applies a mailbox delta in one SQLite transaction (messages, rethread, cursor).

## Sending mail

Agents may prepare a `Draft`. Each `submit_draft` mints a unique pending id. Transmitting mail requires a `UserSendConfirmation` minted with `fell_confirm::InteractiveUi::for_fell_app` (see `fell::confirmation_from_ui`) and bound to that id plus the account, recipients, subject, and body that were shown. A token for draft A does not confirm draft B. `InteractiveUi` is sealed (private field, not `Default`) so a crate that only depends on `fell-core` cannot mint it. The MCP crate has no send tool and does not depend on `fell-confirm`.

An agent draft is a `review_drafts` row (`Store::list_review_drafts`), not a `PendingSend`. Fell reads that list when it is time to show the draft for review. Undo deletes it. `create_review_draft` never calls `submit_draft` or `confirm_send`.

Triage (label, archive, mark read, snooze) writes an `agent_actions` row and the previous message state. `Store::undo` reverses the latest action on the account, including ones an agent took. `Store::undo_actor` reverses only that actor's latest action. Archive and snooze survive a later copy of the same Message-ID.

Per-agent scopes live in `agent_grants` and are off until `fell-mcp grant` (or a later UI) enables them. `read` covers search, reading a thread, and listing labels. `triage` covers the four triage tools and undo of those, and stays off unless that scope is granted. `draft` covers saving and listing review drafts and undo of those. A disabled agent gets an empty tool list. Mailbox changes are rate-limited.

`fell-mcp` speaks MCP on stdio only. It does not listen on a socket. Draft tools (`get_draft`, `edit_draft`, `replace_draft_body`, `request_send`) connect to the compose window's unix socket when Fell is open, so the agent reads the text on screen. That socket is mode `0600` under the user runtime directory. It is not a network listener. `request_send` only focuses the editor. Any future network transport must bind to `127.0.0.1`, authenticate with a token file created mode `0600`, and check the `Origin` header.

Search returns at most 20 hits per page: ids, sender address, date, and a short snippet. Thread bodies are plain text and truncated. Draft bodies are capped when they are saved. Text that came from mail is returned only as `untrusted_email_content`, with ids, sender address, and date outside that object. The model is told to treat that text as data, not as instructions. `list_drafts` defaults to the calling agent's drafts and redacts `bcc` on anyone else's rows. The database file, its WAL file, and its shared-memory file are mode `0600`. See [agents.md](agents.md).

The in-app action log, one-key undo, and an in-app confirm for bulk archive are not part of this server. Neither is binding `in_reply_to` into the send-confirmation snapshot.

## HTML mail

`fell-render` turns HTML into a `StyledDocument` (paragraphs, headings, lists, links, emphasis, quotes, simple tables, images). The same tree is what M3 compose will produce from markdown via `markdown_to_document`, and `fell::render::document_column` paints it. The composer is not in this crate.

Personal and conversational mail stays native. The web view is used only when a bulk signal (List-Unsubscribe, Precedence: bulk or list, or an ESP header) coincides with layout the native painter would break (presentation or nested tables, or heavy CSS). The current store schema has `body_text` and no bulk-header columns, so a row read back with `prepare_stored` stays native. Call `prepare` when the headers are actually known. The fixture corpus under `crates/fell-render/fixtures` locks that choice.

The web view is WebKitGTK through `wry`, dynamically linked. JavaScript is disabled and remote images are removed until the reader allows them. wry 0.57 can child-embed only into an Xlib window, and GPUI on Wayland does not provide one, so WebKit paints in an offscreen window, which is not a mapped toplevel, and the conversation shows that page in the message body. There is no second title bar and no second copy of the subject. The inbox shell toggles that with `MessageBody`; this crate does not register the keymap.

## First platform

Linux on Wayland (Omarchy / Hyprland) is the first target. The `fell` crate enables GPUI's `wayland` and `font-kit` features only.
