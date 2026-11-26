# PR 309 round 16

Mira’s synthetic Gmail-style reply uses three authored lines, two blank lines, a signature and a quote from Jo. The captures use the demo inbox in isolated headless Sway.

| Theme and window | Video | Frames | Web, closed | Web, open |
| --- | --- | --- | --- | --- |
| Tokyo Night, 960 px | [Video](dark-960.mp4) | [Frame strip](dark-960-frames.png) | [Screenshot](dark-960-web-closed.png) | [Screenshot](dark-960-web-open.png) |
| Tokyo Night, 490 px | [Video](dark-490.mp4) | [Frame strip](dark-490-frames.png) | [Screenshot](dark-490-web-closed.png) | [Screenshot](dark-490-web-open.png) |
| Catppuccin Latte, 960 px | [Video](light-960.mp4) | [Frame strip](light-960-frames.png) | [Screenshot](light-960-web-closed.png) | [Screenshot](light-960-web-open.png) |
| Catppuccin Latte, 490 px | [Video](light-490.mp4) | [Frame strip](light-490-frames.png) | [Screenshot](light-490-web-closed.png) | [Screenshot](light-490-web-open.png) |

## Measurements

The recordings contain 2,106 frames at 60 fps. For each captured frame, the reply crop was compared with the two settled renderers. Each strip shows all distinct intermediate reply crops from a recording, with the last settled frame on either side of each swap. The five groups in each strip cover simple → web → simple → web with history closed, then web → simple → web with history open.

The three lines start at y = 186, 232 and 278 in both renderers, with identical horizontal bounds. WebKit’s raster extends 1 px farther at the bottom of each line. The transition crops show one copy of each line in both directions.

| Config | Sampled frames | Card height during every closed swap | Card height during every open swap |
| --- | ---: | ---: | ---: |
| Tokyo Night, 960 px | 523 | 235 px | 330 px |
| Tokyo Night, 490 px | 527 | 235 px | 351 px |
| Catppuccin Latte, 960 px | 531 | 235 px | 330 px |
| Catppuccin Latte, 490 px | 525 | 235 px | 351 px |

These heights are measured between the focused card’s top and bottom border pixels. Pressing `q` expands the native signature and quoted text while the message stays in web view. Pressing it again restores the 235 px card.
