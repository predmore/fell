# Fell UX spec (v1)

Decided with Jon on 2026-11-16. This is the bar the UX judges score against (every score 9.5+), together with docs/scope.md.

## Principles
- Instant: no spinner between a key press and the next frame; the UI only reads the local store.
- Keyboard first, vim grammar; the mouse works but is never required.
- Calm and dense enough to scan: one folder, one list, one conversation.
- Threads are the unit for every list, action and search hit.

## Layout: two panes
- Left: the thread list for the current folder (or search results).
- Right: the open conversation.
- No folder sidebar. Switch folders with `gi` / `gs` / `gd` and a fuzzy folder picker (`gf`, also in the `:` palette).
- A one-line header above the list shows the account, the current folder, and the unread count; a one-line status bar at the bottom shows mode, sync state and which-key hints.
- Pane split is resizable and remembered; `Enter`/`o` focuses the conversation, `Esc`/`h` returns to the list. A key toggles a full-width conversation for long mail.
- Narrow windows (Hyprland tiles): below a width threshold show one pane at a time with the same keys, never a cramped split.

## Folders and threads
- One inbox, no automatic splits or tabs.
- The list shows every thread with at least one message in the current folder, newest activity first.
- Opening a thread shows the whole conversation, including messages that live in other folders (for example your replies in Sent), with each message's folder marked subtly.
- `m` moves the thread out of the current folder: a fuzzy folder picker, then only the messages in the current folder move (Sent copies stay in Sent). `e` archives (move to Archive). Both are undoable with `u` and show a short toast.
- Proton labels stay separate from folders (`l` to label) and show as small chips on the row.

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

## Theme: follow Omarchy
- Read the active Omarchy theme and map its palette (background, foreground, accent, selection, dimmed, error) to Fell's tokens.
- Live-reload when the Omarchy theme changes, with no restart and no flicker.
- Use the system UI and monospace fonts Omarchy configures; respect its font size.
- Fallback when not on Omarchy: a built-in dark and light theme that follow the desktop dark-mode preference.
- All text and markers meet WCAG AA contrast against every shipped Omarchy theme; CI checks the mapping against the stock themes.

## Feedback and states
- Every action gives immediate visual feedback in the same frame; network work happens after.
- Empty folder, first sync, offline and auth-failed states each have a clear one-line message and the key to fix it.
- Errors never block the UI; they appear in the status bar with a key to see details.

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
