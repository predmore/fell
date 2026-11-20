# Independent running-app review

The independently verified release at `7dbd7df48d7385d56cce7effd3034a331b732d1a` scored **9.1/10**. Before images use `628eed87dfbeab9d844a95463c7c5534fd7843e0`. The reviewer verified the release SHA-256 and replayed with synthetic mail in private Weston sessions.

| Running-app comparison | Capture |
| --- | --- |
| Tokyo Night | [Dark before and after](dark-before-after.png) |
| Catppuccin Latte | [Light before and after](light-before-after.png) |
| 500 px | [Narrow before and after](narrow-before-after.png) |
| Keyboard | [Recorded replay](keyboard.mp4) |

The selected result held the same position in all 240 sampled frames of quote editing and recovery. The paused views matched their baselines pixel for pixel in all three layouts. Clicking a current row selected it. Clicking a stale row preserved selection, scroll, and insert focus. The result list scrolled in both states. Keyboard navigation and opening worked, invalid date and quote refinements preserved the results and reader, and both attachment markers remained visible.

Enter on invalid syntax returns to normal mode. Slash resumes editing. A short conversation's date and header rule can shift during repaint on both builds. The review found no blocking UX issue.
