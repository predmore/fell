# Account reconfiguration polish (#328)

Fell ran in headless Sway at 2× scale with a private session bus and loopback-only demo IMAP and SMTP. Dark and light use Tokyo Night and Catppuccin Latte at 1000×760 logical pixels. Narrow uses Tokyo Night at 490×780.

## Footer measurements

The Back-focused and primary-focused screenshots show the same form. Comparing the pixels between each pair gives the two focus-ring boxes. Both labels have flat-bottomed glyphs, and the table records the last solid text row inside each button. Coordinates are in physical screenshot pixels, with inclusive bounds measured from the top left.

| Theme | Logical width | Form | Back / primary baseline y | Back / primary focus-ring y range |
| --- | ---: | --- | --- | --- |
| Tokyo Night | 1000 | Onboarding | 1315 / 1315 | 1269–1342 / 1269–1342 |
| Tokyo Night | 1000 | Edit connection | 1315 / 1315 | 1269–1342 / 1269–1342 |
| Catppuccin Latte | 1000 | Onboarding | 1315 / 1315 | 1269–1342 / 1269–1342 |
| Catppuccin Latte | 1000 | Edit connection | 1315 / 1315 | 1269–1342 / 1269–1342 |
| Tokyo Night | 490 | Onboarding | 1318 / 1318 | 1272–1345 / 1272–1345 |
| Tokyo Night | 490 | Edit connection | 1318 / 1318 | 1272–1345 / 1272–1345 |

## Held changes

Each MP4 starts on the selected account with no held changes. The palette requests a sync after two synthetic pending changes have been seeded in the demo mailbox as held. The row then shows Resend and Discard. Pressing x discards the changes and the two buttons leave.

The Edit connection and Remove pixels are identical before, during the held state, and after discard on all three viewports.
