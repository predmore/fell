# Live search input replay

The 100,000-message synthetic mailbox had 100,000 FTS entries. The timed release build used Asahi Vulkan and nested Weston on Xvfb. Weston reported llvmpipe. The same harness ran the review build and the early-draw build.

FrameLog measures from the delivered key handler through CPU paint. The app prepares the scene after the input callback and GPUI presents it on the compositor callback. Native Hyprland and display presentation remain unverified.

While search was running, paced input had 3 of 155 frames over 8 ms and burst input had 1 of 255 over. The burst p95 was 4.856 ms. The four over-budget frames used 1.546–2.521 ms of UI-thread CPU each.

The screenshots and recording show the settled search UI on nested Weston in Tokyo Night and Catppuccin Latte at 1200 × 780 and in Tokyo Night at 500 × 780. The wide quote preview was also replayed with reduced motion.

Release
fell 0.1.0 (7f5337847567, 2026-11-20)
c5306046d9db96d55b681e149ff14e6339dd2f04f18d5adc8fe3ce983a9ea899  fell
