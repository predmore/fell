# Live search follow-up on 7f53378

Release commit: `7f5337847567001bf004fee3c7458246bda5a697`  
Release SHA-256: `c5306046d9db96d55b681e149ff14e6339dd2f04f18d5adc8fe3ce983a9ea899`

Replayed the release binary against disposable copies of the 100k synthetic mailbox. Fell used Asahi Vulkan on Apple M2. Weston 15.0.1 used llvmpipe under Xvfb. Sway 1.12 used Asahi GLES on a headless output. The Weston replay used XTEST and the same pacing as the earlier evidence. The Sway replay used a virtual Wayland keyboard with 47 ms between the two edits in each pair, allowing the next key to arrive while the previous query was running.

FrameLog measures from handler entry through CPU paint. The table includes keys whose handler started during background search work.

| Replay | Overlapping keys | p95 | Max | Over 8 ms |
| --- | ---: | ---: | ---: | ---: |
| Weston, paced | 152 | 3.392 ms | 26.945 ms | 2 |
| Weston, burst | 282 | 4.856 ms | 9.042 ms | 2 |
| Sway, paced | 154 | 6.006 ms | 11.944 ms | 2 |

The longest frame took 26.945 ms wall time and used 1.818 ms of UI-thread CPU. The 8 ms gate remains open in both environments. These runs measure CPU paint on isolated outputs. Native Hyprland and physical presentation remain unverified. Two isolated Hyprland attempts stopped before Fell launched because Aquamarine requested Wayland protocol versions above those advertised by the parent compositor.

The keyboard replay and screenshots cover grouped results, spelling correction, an operator completion, navigation into a folded quote, recents, and folder restoration. Replayed at 1200 × 780 in Tokyo Night and Catppuccin Latte and at 500 × 780 in Tokyo Night. These captures are for independent UX review.
