# Search syntax refinements and attachment markers

Release commit `f179841d115ece9816c27f353192b24821f4acd8`  
Binary SHA-256 `6e218e16788bdf1d39fc3efda7cddd3611758e27960d46e86c82c40afbcc32f6`

The independent 8.8/10 review on `1f12263` confirmed the three original fixes. It found that incomplete dates and phrases briefly replaced accepted rows with an error and cleared the reader. It also found that search rows omitted the inbox attachment marker.

- The date and quote recordings refine `hall` to `hall after:2026-11-11` and `hall "Mill Lane"` at roughly 85 ms per typed character. The prior excerpt and reader title stay visible through all 152 date frames and all 122 quote frames. An incomplete date shows its format error after idle. An unfinished quote shows its error on Enter.
- Searching `has:attachment` shows the inbox attachment marker on both matching conversations. The selected reader still shows the attachment.
- The existing light and narrow checks were replayed on this release. Tab accepted operator completion, quote navigation opened the folded match, and the Inbox before search and after Escape matched byte for byte.

All UI runs used private Weston/Xvfb sessions and synthetic mail. The independent re-review and the engineering timing gate are tracked separately.
