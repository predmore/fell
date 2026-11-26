# Round 11 web-mail landings

With the local system fonts, WebKit measured the same heights in light and dark schemes on isolated Xvfb and headless Sway. Heights are CSS pixels. The estimate includes the reader’s 12 px allowance. The running app reserves the native link column at its final width, so the link column adds the same height before and after the snapshot arrives.

| Fixture | Scheme | Viewport | Estimate | Painted | Estimate − painted |
| --- | --- | ---: | ---: | ---: | ---: |
| Mobile reveal, stacked columns | Light and dark | 360 | 178.0 | 166.0 | +12.0 |
| Mobile reveal, side-by-side columns | Light and dark | 680 | 77.0 | 69.0 | +8.0 |
| Hover and compact linked paragraphs | Light and dark | 440 | 156.0 | 144.0 | +12.0 |
| Northpine retail newsletter | Light and dark | 600 | 1894.7 | 1889.0 | +5.7 |
| Northpine retail newsletter | Light and dark | 680 | 1315.7 | 1308.0 | +7.7 |
| Ordinary newsletter | Light and dark | 680 | 593.6 | 583.0 | +10.6 |

The paired screenshots show the placeholder on the left and the settled card on the right. The mobile screenshots cover both viewport layouts. The three-panel keyboard screenshots show web view, simple view after pressing `w` at the top, and simple view after relaunch. Both themes stored `web_view = 0` for the demo sender and read the same value after relaunch.

## Bundled-font calibration

The offline test uses a Latin subset of Liberation Sans for both the estimator and WebKit. Its measurements were identical in light and dark and with two different system font configurations. The app still uses system fonts.

| Fixture | Viewport | Estimate | Painted | Estimate − painted |
| --- | ---: | ---: | ---: | ---: |
| inline-styled ESP table | 493 | 391.5 | 383.0 | +8.5 |
| inline-styled ESP table | 560 | 394.5 | 386.0 | +8.5 |
| mobile reveal | 360 | 178.0 | 166.0 | +12.0 |
| mobile reveal | 680 | 77.0 | 69.0 | +8.0 |
| spacer cells and body padding | 440 | 208.0 | 195.0 | +13.0 |
| hero and padded sections | 440 | 858.0 | 846.0 | +12.0 |
| column flex masthead | 440 | 344.0 | 332.0 | +12.0 |
| desktop and mobile pair | 440 | 246.0 | 234.0 | +12.0 |
| desktop and mobile pair | 680 | 332.0 | 320.0 | +12.0 |
| hover and compact paragraphs | 440 | 156.0 | 144.0 | +12.0 |
| retail | 600 | 1894.7 | 1884.0 | +10.7 |
| retail | 680 | 1315.7 | 1305.0 | +10.7 |
| ordinary newsletter | 680 | 593.6 | 582.0 | +11.6 |
| long article | 440 | 1325.0 | 1309.0 | +16.0 |
