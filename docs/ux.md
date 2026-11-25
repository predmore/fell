# Fell UX spec (v1)

Decided with Jon on 2026-11-16. This is the bar the UX judges score against (every score 9+), together with docs/scope.md.

## Principles
- Instant: no spinner between a key press and the next frame; the UI only reads the local store.
- Keyboard first, vim grammar; the mouse works but is never required.
- Calm and dense enough to scan: one folder, one list, one conversation.
- Threads are the unit for every list, action and search hit.
- Mail and agents together: an agent's drafts, notes and suggestions sit in the same lists and conversations as mail, reached with the same keys.

## Motion: quick, never jumpy
- Nothing jumps. Content that appears, leaves, grows or moves transitions instead of popping: rows slide or fade, panes and overlays fade and scale slightly, scrolls glide, counts and toasts cross-fade.
- Transitions are quick: 80–160 ms with an ease-out curve. Input never waits for one; a key press during a transition jumps straight to the new state.
- No layout shift: reserve space for text that is still loading, keep row heights stable, and keep the selection where the eye is.
- Animation is off when `reduce_motion` is set or the desktop asks for reduced motion. It never pushes a frame past the key-to-frame budget.
- In the code: `crates/fell/src/motion.rs` holds the curve, the named durations and the helpers every view uses. The `reduce_motion` preference in Settings applies as soon as it changes. Without an explicit preference, `FELL_REDUCE_MOTION=1` reduces motion and `=0` forces motion on. With neither override, `gtk-enable-animations=false` in the GTK settings turns it off. A key ends fades in and overlay openings at once; what is leaving (a notice, a closed overlay) keeps fading out and takes no input. A scroll or a row that is still moving starts its next move from where it is drawn instead of snapping first; what the key acts on is already the final state. Everything that leaves fades out over `FADE` (120 ms), as long as it took to come in, so an exit never reads as a cut. Content that replaces other content in place (another thread, another folder) arrives from 40 % opacity, never through an empty frame.

## Quiet: no noise
- Do not print key hints for obvious actions (`Esc` to close, `Enter` to open, `j`/`k`, `q` to quit). An empty state says what is true ("No mail matches"), not how to leave it.
- Show a key only where it is the answer to a question the user has right now and they could not guess it: a fix for an error, a pending agent suggestion, a destructive confirm. Discovery lives in which-key and the `:` palette.
- One message at a time in the status bar. A replacement notice starts fading in after the old notice fades out. Notices also fade out on their own.

## Layout: two panes
- Left: the thread list for the current folder (or search results).
- Right: the open conversation.
- No folder sidebar. Switch folders with `gi` / `gs` / `gd` and a fuzzy folder picker (`gf`, also in the `:` palette).
- A one-line header above the list shows the account, the current folder, and the unread count; a one-line status bar at the bottom shows mode, sync state and, after the leader, which-key hints. It shows no key hints at rest; a count or a chord's prefix shows while it is typed.
- Pane split is resizable and remembered; `Enter`/`o` focuses the conversation, `Esc`/`h` returns to the list. A key toggles a full-width conversation for long mail.
- Narrow windows (Hyprland tiles): below a width threshold show one pane at a time with the same keys, never a cramped split.

## Folders and threads
- One inbox, no automatic splits or tabs.
- The list shows every thread with at least one message in the current folder, newest activity first.
- Opening a thread shows the whole conversation, including messages that live in other folders (for example your replies in Sent), with each message's folder marked subtly.
- `m` moves the thread out of the current folder: a fuzzy folder picker, then only the messages in the current folder move (Sent copies stay in Sent). `e` archives (move to the account's Archive, or All Mail where there is none). Folders show by role: a server's `\Sent` folder reads as Sent whatever its name, and `gs` opens it. Both are undoable with `u` and show a short toast.
- `#` moves the thread's messages in the current folder to Trash, and `!` to Spam. In Spam, `!` moves them back to the Inbox. Both act on the `x` or `V` selection, slide the remaining rows into place, and support undo with `u`. In Trash, `#` shows a warning for the subject or marked count. `y` removes those copies from Fell and any other key dismisses the confirmation. The server copies stay in Trash and undo can still restore unrelated changes.
- `b` opens Snooze for the current thread or the `x` or `V` selection. Choose later today, tomorrow morning, or next week, or type a local time such as `mon 9am`. `gz` lists snoozed threads with their wake times. `B` clears a snooze. At its wake time, the thread returns to the top of Inbox unread if it still belongs there.
- `M` mutes or unmutes the selection. Muted threads and their new messages are hidden from Fell’s Inbox and desktop alerts. `gm` lists muted threads, and `M` brings them back. Snooze, unsnooze, and mute are undoable with `u`.
- Labels stay separate from folders and show as small chips on the row. `l` opens a fuzzy picker for the current thread or the `x` or `V` selection. Picking a checked label removes it. A partially applied label is added to the rest of the selection. Typing a new name offers to create and apply it. `u` undoes the whole edit. `gl` or `Space f l` opens a label’s thread list.

## Thread list rows: comfortable, two lines
- Line 1: participants (you shown as "me", latest sender emphasized), message count when > 1, attachment and star markers, relative date right-aligned.
- Line 2: subject, then a dimmed snippet of the latest message, truncated on one line.
- Unread threads use bold weight and an accent marker; read threads are regular weight.
- Selection is a full-row highlight; visual-range selection highlights every selected row.
- Stable height rows, no layout shift while syncing; new mail slides in without moving the selection.

## Conversation view
- Messages stacked oldest to newest; read messages collapsed to one line (sender, snippet, date), unread and the latest expanded.
- Quoted history and signatures collapsed by default with a key to expand.
- Native styled text by default; a key toggles to the web view for complex HTML; remote images blocked with a one-line, per-sender allow prompt.
- `j`/`k` move between messages when the conversation has focus; `Ctrl-d`/`Ctrl-u` page (Space is the leader).
- Attachments wrap beneath each expanded message. `a` focuses its files, `h`/`l` move between them, and `Esc` returns to the message. `Enter` or `o` opens the focused file in the desktop's default app. `S` saves it to Downloads, and `Space a s` saves all files on the current message. `Space a R` retries a failed attachment action.
- The reader shows stored text immediately. After 100 ms it names any parts still loading in the recipient row. A visible label stays for 300 ms or until 150 ms after the body arrives, whichever comes first. “Loading formatted version…” means the stored text is already visible. If the read fails or the server is offline, the row names what is missing and shows the sync retry key. Files appear with the body, or ahead of it when the part list is known and the body is still loading after 400 ms. Active downloads and failed files use the size slot for their status. Focusing a failed file shows its retry key in the status bar.

- Agent notes sit above the messages. Open the conversation to see a note's suggestion keys, use `Space a n` to expand or collapse it, and `Space a x` to dismiss it. `Space a t` opens Threads with suggestions. An outlined marker in the list shows a note you haven't seen.

## Theme: follow Omarchy
- Read the active Omarchy theme and map its palette (background, foreground, accent, selection, dimmed, error) to Fell's tokens.
- Live-reload when the Omarchy theme changes, with no restart and no flicker.
- Use the system UI and monospace fonts Omarchy configures; respect its font size.
- Fallback when not on Omarchy: a built-in dark and light theme that follow the desktop dark-mode preference.
- All text and markers meet WCAG AA contrast against every shipped Omarchy theme; CI checks the mapping against the stock themes.

## Feedback and states
- Every action gives immediate visual feedback in the same frame; network work happens after.
- Empty folder, first sync, offline and auth-failed states each have a clear one-line message; offline and auth-failed also name the fix.
- Errors appear in the status bar without blocking input. If Fell crashed, the next inbox launch shows “Fell crashed last time · Space ? c to view” once. The crash report stays available from the command palette and the `Space ?` group after the notice fades.
- If a sync or mailbox worker panics, the status bar shows which worker is restarting. An interrupted request returns an error. The worker resumes queued work after a short delay. `Space ? l` opens the diagnostic log.

## Evidence for UX judges
- Screenshots of the running app on Hyprland-sized windows, on at least two stock Omarchy themes (one dark, one light), plus a narrow-window shot.
- A short recording for any keyboard flow (navigate, open, move, undo).

## Leader key, chords, which-key and command palette
- Leader key defaults to `Space` and is configurable in the keymap file.
- Chords live under the leader, grouped by mnemonic, for example `Space f` folders (`Space f i` inbox, `Space f m` move), `Space s` search, `Space t` toggles (web view, theme, density), `Space a` agents (M4).
- Which-key: after the leader or any chord prefix, a popup lists the next keys and their actions within one frame of a short delay (default 300 ms, configurable; 0 shows it immediately). `Esc` cancels; `Backspace` steps back a level.
- Direct vim keys (`j`, `e`, `gi`...) stay as they are; the leader is for less frequent actions and discovery, not a replacement.
- Command palette (`:` and `Ctrl-Shift-p`): fuzzy search over every action, showing each action's key binding; recent actions first; runs on Enter with the current selection as the target.
- Every action is reachable from the palette, and every chord shows in which-key, so nothing is hidden behind a memorized key.
- `Space ,` or the Settings palette entry opens preferences. Use `j` and `k` to select a section or row, Enter or Space to open or change it, and `h` and `l` to cycle values. Each preference has a palette entry that opens its row.

## Compose: styled markdown you edit as plain text
- `c` in the inbox (and Compose in the palette) opens the compose window on a new draft for the account, starting on the To line. There is one compose window: opening another draft moves it there. `gd` lists your drafts, newest first, above the synced Drafts folder; `Enter` opens the selected one in the editor. Closing the window keeps the draft and the inbox says so ("draft saved · gd drafts"). With write-back enabled, Fell saves an open draft to the account’s Drafts folder after about five seconds of inactivity and on close. Open a draft from another mail client with `gd` and `Enter`. A confirmed send clears its server draft when write-back is enabled.
- After a successful send, the compose window closes, the inbox shows "Sent" with the subject, and the draft leaves the drafts list. A sent reply fades into the open conversation and keeps the reader anchored. A server copy that needs retrying gets a short status notice. After a rejection the draft stays open for editing, with `:details` for the server's reply. An uncertain send keeps the draft locked and points to Sent. A matching copy confirms the send, and a missing copy leaves delivery unknown. `:unlock` asks the person to accept the duplicate risk before restoring editing and Send.
- The composer is a markdown editor that styles as you type. The text you edit is always plain markdown (no hidden rich-text model, no toolbar needed), but headings, bold, italics, code, lists, quotes and links render styled in place, so the draft looks like the email the recipient will get.
- Markdown markers (`**`, `#`, `>`, link URLs) stay editable: they show dimmed on the line the cursor is on and fold away elsewhere. Cursor movement, selection, undo and vim motions behave exactly as in plain text.
- Inline images render in place, at a sensible max width. Paste or drag an image (or attach with a key) and it inserts a markdown image reference that displays as the image; deleting the reference removes the image. Images go out as inline CID parts, not links to anywhere.
- Replies and forwards show the quoted original styled and collapsed below the draft, expandable, and keep its inline images.
- `r` replies, `R` replies all, and `f` forwards. In the conversation they answer the message under the cursor; from the list, the latest message. A reply goes to the sender, or to Reply-To when the message has one; reply all adds the other To and Cc recipients. Your own address is never added. The subject gets one `Re:` or `Fwd:`, never a second. A reply starts in the body; a forward starts on the empty To line. The quoted original is sent after what you write. A forward carries the original's attachments within the compose limits and names any omitted files in the editor and quoted header.
- A key toggles raw source view (no styling) for anyone who wants it; styled is the default.
- Sending produces multipart/alternative: an HTML part rendered from the markdown with a small, conservative stylesheet that looks right in Gmail, Outlook and Proton, plus a text/plain part that is the markdown itself. Inline images go in multipart/related. A preview key shows the rendered HTML before send.
- The rendering path is shared with the conversation view's native renderer so composed and received mail look consistent.
- Judges need screenshots of a draft with headings, lists, a link, a quote and an inline image in dark and light themes, plus a recording of pasting an image and editing around it.
