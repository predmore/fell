# Round 11 web-mail landings

WebKit measured the same heights in light and dark schemes on isolated Xvfb and headless Sway. Heights are CSS pixels. The estimate includes the reader’s 12 px allowance. The running app reserves the native link column at its final width, so the link column adds the same height before and after the snapshot arrives.

| Fixture | Scheme | Viewport | Estimate | Painted | Estimate − painted |
| --- | --- | ---: | ---: | ---: | ---: |
| Mobile reveal, stacked columns | Light and dark | 360 | 178.0 | 166.0 | +12.0 |
| Mobile reveal, side-by-side columns | Light and dark | 680 | 77.0 | 69.0 | +8.0 |
| Hover and compact linked paragraphs | Light and dark | 440 | 156.0 | 144.0 | +12.0 |
| Northpine retail newsletter | Light and dark | 600 | 1894.7 | 1889.0 | +5.7 |
| Northpine retail newsletter | Light and dark | 680 | 1315.7 | 1308.0 | +7.7 |
| Ordinary newsletter | Light and dark | 680 | 593.6 | 583.0 | +10.6 |

The paired screenshots show the placeholder on the left and the settled card on the right. The mobile screenshots cover both viewport layouts. The three-panel keyboard screenshots show web view, simple view after pressing `w` at the top, and simple view after relaunch. Both themes stored `web_view = 0` for the demo sender and read the same value after relaunch.
