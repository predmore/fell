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
| `fell-confirm` | Sealed `InteractiveUi` (private field, not `Default`). Required to mint a send confirmation. The GPUI app depends on it; `fell-mcp` must not. |
| `fell` | GPUI application. Opens a Wayland window. Depends on `fell-core` for data, not the other way around. Mints send confirmations. |
| `fell-mcp` | MCP server. Search, read threads, list labels, triage, and review drafts. No send tool. Scopes stay off until enabled. |

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

Archive, label, delete, and undo update this SQLite file only. There is no socket, no IMAP session, and no send.

The thread is the unit. Archive moves the folder the caller names (the folder on screen) to `Archive` on each message in the thread that has it. Any other folder on that row, such as `Sent`, stays. Delete moves that folder to `Trash`. Delete from `Trash` drops `Trash`, and removes the row when it was the last folder. Labels are a separate list. Each successful call stores the previous folders and labels on that account's undo stack. `Store::undo` restores the latest entry for that account and leaves other accounts as they are.

`in:` and `label:` read the columns those calls write. Folder and label words enter and leave the trigram vocabulary with the change. `apply_delta` still unions other folders and labels for the same Message-ID, and leaves out a folder archive or delete removed. Undo restores that folder and then drops the saved message copy, including the body and BCC. Outbound sync of these edits is separate.

## Threading

A thread id is assigned when a message is stored, then the account is re-resolved in memory from threading columns (not bodies):

1. If Proton sent a conversation id, that is the thread key (`proton:…`).
2. Otherwise join via `In-Reply-To` / `References` to an already-stored parent in the **same account**.
3. Otherwise start a thread from the message's own `Message-ID`.

Messages are unique on `(account_id, message_id)`. A second copy with the same id (a Sent copy, or the same message delivered to several aliases) stays one row and unions folders, labels, arrival addresses, and recipients, and keeps an attachment if either copy had one. Copies with a different Message-ID stay separate. There is no body-fingerprint collapse.

After a delta with messages is written, each row's previous thread and its header-parent thread are unioned. A `proton:` id absorbs a non-proton header thread. Two different `proton:` ids are not unioned. Non-proton merges keep the earliest already-assigned thread id, so the first id handed back still lists the messages after a missing parent arrives. Cursor-only deltas skip the scan. A thread-id-only update does not rewrite FTS (the update trigger is `OF` subject, from name, from email, body, snippet, and the recipient columns). A delta whose rows belong to another account is rejected and does not advance the cursor.

Proton conversation ids are preferred so Bridge and the future API stay consistent with Proton's own threading, and two Proton conversations stay two threads. Sync applies a mailbox delta in one SQLite transaction (messages, rethread, cursor).

## Sending mail

Agents may prepare a `Draft`. Each `submit_draft` mints a unique pending id. Transmitting mail requires a `UserSendConfirmation` minted with `fell_confirm::InteractiveUi::for_fell_app` (see `fell::confirmation_from_ui`) and bound to that id plus the account, recipients, subject, and body that were shown. A token for draft A does not confirm draft B. `InteractiveUi` is sealed (private field, not `Default`) so a crate that only depends on `fell-core` cannot mint it. The MCP crate has no send tool and does not depend on `fell-confirm`.

An agent draft is a `review_drafts` row (`Store::list_review_drafts`), not a `PendingSend`. Fell reads that list when it is time to show the draft for review. Undo deletes it. `create_review_draft` never calls `submit_draft` or `confirm_send`.

Triage (label, archive, mark read, snooze) writes an `agent_actions` row and the previous message state. `Store::undo` reverses the latest action on the account, including ones an agent took. `Store::undo_actor` reverses only that actor's latest action. Archive and snooze survive a later copy of the same Message-ID.

Per-agent scopes live in `agent_grants` and are off until `fell-mcp grant` (or a later UI) enables them. `read` covers search, reading a thread, and listing labels. `triage` covers the four triage tools and undo of those. `draft` covers saving and listing review drafts and undo of those. A disabled agent gets an empty tool list.

## First platform

Linux on Wayland (Omarchy / Hyprland) is the first target. The `fell` crate enables GPUI's `wayland` and `font-kit` features only.
