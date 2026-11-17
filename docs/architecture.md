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
| `fell-core` | Accounts, backend trait, sync, local SQLite store (FTS5 and a local body-embedding index), threading, search, send-confirmation types. No UI. |
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

Misspellings use a character-trigram index of words from subject, sender, recipients, body, folders, and labels. Damerau-Levenshtein confirms a candidate. When the typed query matches nothing and the corrected query matches, the response carries those hits and `did_you_mean`. Keyword search does not read the local embedding index.

## Local embeddings

Each message body has one row in `message_embeddings` in the mailbox SQLite file (the same `mail.db` the account's messages already use), keyed by account id and message id. Deleting that file deletes the index. Deleting a message deletes its row in that transaction. A body change drops a stale row. The row stores a model id, a model version, and a little-endian `f32` blob.

The embedder is the `BodyEmbedder` trait. The one wired up now is `local-hash-v1` version `1`, a placeholder lexical feature hash of `body_text` (or the snippet when the body is null). It is not a semantic model. There are no weights, bundled or downloaded, and no extra embedding crate. A later model uses the same trait. Rows whose id or version differ are rewritten by the background indexer.

`Store::open` starts a background thread that fills missing or outdated rows, recent mail first, after taking an exclusive lock on a sidecar of the mailbox path actually opened. A symlink opens the canonical file, not a second name, so it does not get its own `-wal`. A second hard link uses the path recorded on the file. The database is opened only on that one path. The exclusive lock stays granted until the store that took it is dropped; it is not converted to a shared lock. If the filesystem cannot store the path or the lock, and this is the only name for the file, the canonical file still opens. The open fails only when another live opener's path cannot be recovered and this caller's name would be a second WAL. Closing the store stops the worker between messages and wakes it out of a lock wait. If that lock cannot be created, the mailbox still opens. A process that finds the lock held keeps trying and starts indexing when the lock is free, so closing the other mailbox does not leave the queue unindexed. The worker drops the lock when it stops. Hashing happens before the write transaction; the transaction only stores rows whose body is still the one that was hashed. A second process does not index while the lock is held. New and invalidated messages are queued and indexed before the historical walk. A pass walks from newest to oldest and resumes after the last row it stored. Mail newer than that floor is paged until a short batch shows the range is done, so a sync burst is not skipped. Each batch is one immediate transaction, then the thread yields so sync and `fell-mcp` can write. Message saves, deletes, and triage also take the write lock at the start of their transaction, so a batch commit cannot invalidate a snapshot they already read. When a pass finds nothing, the thread waits at least a second and backs off. A lock conflict retries after a short fixed delay and does not extend that wait. Open, listing threads, and keyword search do not compute vectors. Nothing leaves the machine. There is no cloud embedder. This index is not semantic search.

`Store::search` does not read this table. Hybrid ranking is later M5 work.

## Local actions

Archive, move, label, delete, and undo update this SQLite file only. There is no socket, no IMAP session, and no send.

The thread is the unit. Archive moves the folder the caller names (the folder on screen) to `Archive` on each message in the thread that has it. Move does the same to the folder the caller names. Any other folder on that row, such as `Sent`, stays. Delete moves that folder to `Trash`. Delete from `Trash` drops `Trash`, and removes the row when it was the last folder. Labels are a separate list. Each successful call stores the previous folders and labels on that account's undo stack. `Store::undo` restores the latest entry for that account and leaves other accounts as they are.

`in:` and `label:` read the columns those calls write. Folder and label words enter and leave the trigram vocabulary with the change. `apply_delta` still unions other folders and labels for the same Message-ID, and leaves out a folder archive or delete removed. Undo restores that folder and then drops the saved message copy, including the body and BCC. Outbound sync of these edits is separate.

## Threading

A thread id is assigned when a message is stored, then the account is re-resolved in memory from threading columns (not bodies):

1. If Proton sent a conversation id, that is the thread key (`proton:…`).
2. Otherwise join via `In-Reply-To` / `References` to an already-stored parent in the **same account**.
3. Otherwise start a thread from the message's own `Message-ID`.

Messages are unique on `(account_id, message_id)`. A second copy with the same id (a Sent copy, or the same message delivered to several aliases) stays one row and unions folders, labels, arrival addresses, and recipients, and keeps an attachment if either copy had one. A different Message-ID collapses only when the body fingerprint is exactly the same, the sender is the same, both dates are non-zero and within six hours, at least one copy has `List-Id`, and the Proton conversation ids are absent or equal. The fingerprint is only of non-empty body text. A missing body has no fingerprint, and subject is not an identity. More than one matching row stays as separate messages. A row that already remembers another Message-ID is not a collapse target for a later message. The merge keeps every body, parent, subject, proton id, folder, label, address, star, archive bit, and snooze already stored, prefers the non-list subject, display name, and `In-Reply-To`, and copies a proton conversation id onto a row that lacks one. An upgrade joins duplicate fingerprints in batches, without loading every body at once. A pair stored before `list_id` existed joins only when one copy is in Sent and the other subject has a list tag for that same subject. Two plain messages with the same body stay apart. The account is rethreaded before schema version 3 is stamped so a reply to a deleted copy stays with the surviving row. Later mail still needs a list id on one side. A same-id resync replaces `In-Reply-To` and `References` when the incoming copy has them. When that resync is not a list rewrite of a row that already absorbed the other copy, a non-empty incoming subject, display name, and Proton conversation id replace the stored ones. An empty incoming Proton id still keeps the stored one. A later sync of the list copy does not wipe a body, parent, subject, or Proton id the other copy filled in. The other Message-ID is remembered so a reply to it still joins the row.

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
