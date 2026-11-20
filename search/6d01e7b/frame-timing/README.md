# Live search frame timing

The 100,000-message synthetic mailbox had 100,000 FTS entries. This replay used the release build on aarch64 with Asahi Vulkan and nested Weston on Xvfb. Weston reported llvmpipe.

FrameLog measures from the delivered key handler through CPU paint. Native Hyprland and display presentation remain unverified. The summary counts key frames whose handler started during measured search work.

Paced input had six of 154 overlapping frames over 8 ms. Burst input had 256 of 271 over 8 ms.

Release
fell 0.1.0 (6d01e7b14925, 2026-11-20)
5ac3a9071ace0d021a48deed0b56bbd5ef4fc2ba87a8d183800cdd3bc4fd5e99  fell
