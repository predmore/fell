# Fell v1 product scope

Settled with Jon on 2026-11-15.

## Principles
- Local first: the UI only reads the local store; the network never blocks a keystroke.
- Threads are the unit: lists, actions, search hits and agent tools work on conversations.
- Keyboard first: every action has a key, vim grammar by default, mouse optional.
- Private by default: indexes, embeddings and agent access stay on the machine unless the user opts in.
- Agents assist, the user sends: agents can read, search, triage and draft; only a keypress in Fell's UI sends mail.

## Pillars
1. **Fast.** Opens to the inbox from the local store, renders every action on the next frame, syncs in the background. Speed budgets below are enforced by CI benchmarks.
2. **IMAP/SMTP, any account.** IMAP sync with IDLE and incremental resync (CONDSTORE/QRESYNC where supported); SMTP send; drafts saved to Drafts. Proton Bridge is the primary test target, but any IMAP account works in v1 (Gmail, Fastmail, Dovecot...), including OAuth for Gmail and Outlook and per-provider quirks (e.g. Gmail labels). Multiple accounts and per-address identities. Labels/folders map so one message can carry several labels and stay one thread. A direct Proton API backend comes later behind the same trait.
3. **Excellent threading.** JWZ on Message-ID/References/In-Reply-To with careful subject fallback; threads span folders so Sent replies appear inline; collapse read messages and quoted text; manual split/merge; mute. **Duplicates collapse to one message:** Sent copy plus mailing-list/alias echo, and copies delivered to several aliases, show once (Message-ID first, body fingerprint when a list rewrites headers); the merged message shows every folder, label and address it arrived through. A messy real-world thread corpus guards regressions.
4. **Offline indexing.** Full bodies and headers in local SQLite; incremental indexing off the UI thread, recent mail first, progress shown. Attachment text extraction later.
5. **Search.** FTS5 + BM25 grouped by thread; operators `from: to: subject: in: label: has:attachment is:unread before: after:`; misspelling tolerance via trigram index + did-you-mean; semantic search with a **local embedding model by default**, hybrid-ranked with keyword results. A cloud embedding API is an explicit per-account opt-in that warns message text leaves the machine. Results update as you type.
6. **Keyboard first, vim style.** Normal / insert (compose, search) / visual modes; counts and dot where sensible; user keymap file like Zed's; which-key hints after a leader key.
7. **Agents via MCP.** MCP server first: search, read threads, list labels; triage (label, archive, mark read, snooze) logged and undoable; drafts that appear in Fell for review. **No send tool.** Per-agent scopes (read / triage / draft), off until enabled. In-app agent panel comes after v1 using the same tools.

## HTML email
Render HTML as native styled text in GPUI wherever possible (paragraphs, headings, lists, links, emphasis, quotes, simple tables, inline images). Personal and conversational mail always renders natively. The embedded web view is mainly for marketing and newsletter mail. Fall back only when bulk-mail signals (List-Unsubscribe, Precedence: bulk or list, ESP headers) combine with layout complexity (table-based layout, heavy CSS). Web view is WebKitGTK via `wry` or similar, dynamically linked; must stay MIT-compatible and pass cargo-deny, with JavaScript disabled and remote images blocked until allowed per sender. A key toggles between the native and web views. The heuristic stays conservative: prefer native unless conversion would visibly break the message.

## Speed budgets (mid-range laptop, to tune once benchmarks exist)
| Action | Budget |
|---|---|
| Cold start to usable inbox | 300 ms |
| Key press to rendered frame | 8 ms |
| Open a 50-message thread | 16 ms |
| Keyword search over 100k messages | 50 ms |
| Semantic search over 100k messages | 150 ms |
| Memory at rest, 100k messages | 250 MB |

## Default keymap
`j`/`k`, `gg`/`G` move; `Enter`/`o` open; `x`, `V` select / visual range; `e` archive, `#` delete, `!` spam; `m` move the thread's messages out of the current folder (undoable); `r`, `R`, `f` reply, reply all, forward; `c` compose; `l` label, `s` star; `gi`, `gs`, `gd` inbox, sent, drafts; `gf` fuzzy folder picker; `/`, `n`/`N` search; `:` command palette; `u` undo (including agent actions).

The move action itself ships in M3. The M1 inbox PR only leaves room for it.

## Out of scope for v1
Direct Proton API backend; calendar, contacts, rules/filters; rich HTML compose (v1 composes plain text and Markdown); in-app agent panel; mobile/Windows/macOS builds (core stays portable; Linux on Hyprland is the supported target); PGP for non-Proton recipients beyond what Bridge does.

## Packaging
Linux x86_64. Flatpak, .deb, and .rpm are skipped for now.

- **AUR**, by M6, not now: `fell-git` builds `main`; `fell-bin` wraps tagged releases.
- **GitHub Release tarball** per tag, by M6: an x86_64 Linux binary, a `.desktop` file, and an icon.
- **AppImage** per tag, by M6. Depend on the system WebKitGTK library rather than bundling it. `wry` (or similar) stays dynamically linked, so the LGPL library stays out of the crate graph and out of the release artifact. That keeps Fell MIT-compatible and cargo-deny-clean. Native rendering needs no WebKitGTK; the newsletter fallback needs the distro's WebKitGTK package installed.
- **Nightly CI from `main`**, in M1, as its own small PR: the tarball and the AppImage as downloadable artifacts, so Jon can test without compiling.

## Milestones
- **M0 Foundation**: merged in #1.
- **M1 Read your mail**: read-only IMAP sync into the store (Bridge first, generic IMAP path), threaded inbox, conversation view with native HTML-to-text rendering and web view fallback, vim navigation. Fix #2 and #3.
- **M2 Find anything**: full-text search with operators, typo tolerance, offline indexing at scale.
- **M3 Act and reply**: archive, move, label, delete, undo, compose/reply, SMTP send; other-provider support incl. OAuth. The move action itself (`m`) ships here; the M1 inbox PR only leaves room for it.
- **M4 Agents**: MCP server with search, read, triage, draft tools, scoped access, action log.
- **M5 Semantic search**: local embeddings, hybrid ranking, opt-in cloud embeddings.
- **M6 Daily driver**: budgets enforced in CI, keymap config, AUR (`fell-git` and `fell-bin`), GitHub Release tarball, and AppImage.

## Review gates
UX quality is a top priority. Any PR that changes something the user sees or touches (UI, keymap, rendering, interaction, performance feel) merges only when, in addition to CI and the code reviewer:

- Independent UX judges who did not write the code each score it 9.5 or better out of 10. [docs/ux.md](ux.md), together with [docs/scope.md](scope.md), is the bar they score against (Zed-like speed and polish, keyboard-first vim grammar, typography and density, clarity, focus states, empty and loading states, error handling).
- The PR description attaches screenshots of the running app (before/after when changing existing UI, several window sizes) and a short recording for keyboard flows. Those are the same images the judges score. For the inbox, that evidence is screenshots of the running app on a dark stock Omarchy theme, a light stock Omarchy theme, and a narrow window, plus a keyboard recording.

Applies from M1's inbox PR onward.
