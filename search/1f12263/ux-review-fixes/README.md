# Live search UX fixes

Release commit `1f122633912fa15a2a9d1e030a931b232a6ca5f0`  
Binary SHA-256 `7641050025c49ee73fd3f91bbbfc72c4ee045e3d86f32c57f85170b9eb72c547`

The independent UX review scored `7f53378` 8.4/10. It found empty-state flashes while typing, insufficient contrast in light-theme match highlighting, and missing sender and conversation counts on results.

- The dark recording refines `hall` to `hall venue` at about 105 ms per key. The excerpt and reader title are both visible in all 122 decoded frames. The submitted nonmatching query shows the settled `No mail matches` state.
- The Catppuccin Latte screenshot shows unselected matched text `#1A58D5` on `#D1DDF3` at 4.51:1 and selected text `#3B3E52` on `#B8BCC7` at 5.54:1. The stock-theme test checks the two highlight pairs across the complete palette set.
- At 500 px the result row shows `Priya Natarajan · 3` and the highlighted excerpt. A second Enter opens the full-width reader, `n` reveals the quote, and `h` returns to results. The pre-search and post-Escape Inbox captures match byte for byte.

The replays used synthetic mail, isolated XDG state, and nested Weston/Xvfb. This packet is for independent UX review. The product timing gate is tracked separately.
