# Architecture

Fell is a native Proton Mail client. The UI never talks to the network; it reads a local store that a sync layer fills from a mail backend.

```text
┌─────────────┐     ┌─────────────┐     ┌──────────────────┐
│  fell (GPUI)│     │   fell-mcp  │     │ Proton Mail      │
│  Wayland UI │     │  agent stub │     │ Bridge (IMAP)    │
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
| `fell-confirm` | ZST `InteractiveUi`. Required to mint a send confirmation. The GPUI app depends on it; `fell-mcp` must not. |
| `fell` | GPUI application. Opens a Wayland window. Depends on `fell-core` for data, not the other way around. Mints send confirmations. |
| `fell-mcp` | Stub MCP server for later agent access. Tools: search, summarize, triage, draft. No send tool. |

## Backends

`MailBackend` is the seam. Two implementations are planned; both are stubs in this foundation:

1. **Bridge** — local IMAP/SMTP against [Proton Mail Bridge](https://proton.me/mail/bridge). First backend.
2. **Proton API** — direct Proton HTTP API. Same trait, later.

The UI and MCP crates take `dyn MailBackend` (or a store already populated by sync). They do not import IMAP or Proton protocol code.

## Local store

SQLite is the source of truth for the UI. Sync writes messages, threads, and an FTS5 index. Reads do not block on the network. If Bridge or the API is down, the last synced mailbox still renders.

## Threading

A thread id is assigned when a message is stored, then the account is re-resolved in memory from threading columns (not bodies):

1. If Proton sent a conversation id, that is the thread key (`proton:…`).
2. Otherwise join via `In-Reply-To` / `References` to an already-stored parent in the **same account**.
3. Otherwise start a thread from the message's own `Message-ID`.

Messages are unique on `(account_id, message_id)`. After a delta with messages is written, each row's previous thread and its header-parent thread are unioned: Proton conversation ids win; otherwise the already-assigned thread is kept so a caller-held id still lists the messages. Cursor-only deltas skip the scan. A thread-id-only update does not rewrite FTS (the update trigger is `OF` subject, sender, and body). A delta whose rows belong to another account is rejected and does not advance the cursor.

Proton conversation ids win when present so Bridge and the future API stay consistent with Proton's own threading. Sync applies a mailbox delta in one SQLite transaction (messages, rethread, cursor).

## Sending mail

Agents may prepare a `Draft`. Each `submit_draft` mints a unique pending id. Transmitting mail requires a `UserSendConfirmation` minted with `fell_confirm::InteractiveUi` (see `fell::confirmation_from_ui`) and bound to that id plus the recipients and body that were shown. A token for draft A does not confirm draft B. The MCP crate has no send tool and does not depend on `fell-confirm`, so it cannot construct that token.

## First platform

Linux on Wayland (Omarchy / Hyprland) is the first target. The `fell` crate enables GPUI's `wayland` and `font-kit` features only.
