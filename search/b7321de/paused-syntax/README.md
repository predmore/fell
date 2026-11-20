# Search results during paused syntax

Release commit `b7321dec8c9459b44fc1d7460a0698cca721ca3e`  
Binary SHA-256 `189b154fb784023cbd9009d8403c36fec9980ae40f81a57cd06ca454c07ef453`

The independent UX review scored f179841 8.9/10. Pausing mid-date or mid-phrase cleared the excerpt and reader. Attachment markers and the preceding fixes were verified.

- Starting from `hall`, the paired replays pause 450 ms at `hall after:2026` and `hall "Mill` before completing the query. On the new release, the excerpt and reader title remain visible in every decoded frame of both recordings (212 date, 152 quote). The status bar shows the format hint and the list labels the retained rows as previous results.
- Submitting an unfinished quote with Enter shows the error in the status bar. A valid expression with no matching mail still settles to `No mail matches`.
- Both attachment results show the Inbox marker in Tokyo Night, Catppuccin Latte, and at 500 px. All captures were made with synthetic mail and isolated Weston/Xvfb sessions.

The separately paced 100k-message replay used Asahi Vulkan under nested Weston/Xvfb. Five of 153 input samples during query work exceeded 8 ms from the handler to CPU paint. Nearest-rank p95 was 4.763 ms and the maximum was 29.853 ms. Native Hyprland and physical presentation remain unverified.
