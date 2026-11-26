# Round 13 web mail

PR head `5bf2b5540bf314b286154c823c37dbfe497e7f5e`.

The comparison images show the placeholder on the left and the settled card on the right. Each half is a 960 × 860 Fell window with a 530 px mail pane. The card bottom is the last row of card fill at the left inset, measured in screenshots at 1× scale.

| Fixture | Theme | Placeholder bottom | Settled bottom | Movement |
| --- | --- | ---: | ---: | ---: |
| [Fixed-width article, dark](article-dark-landing.png) · [Recording](article-dark-landing.mp4) | Tokyo Night | 684 px | 672 px | 12 px up |
| [Fixed-width article, light](article-light-landing.png) · [Recording](article-light-landing.mp4) | Catppuccin Latte | 684 px | 672 px | 12 px up |
| [SendGrid, dark](sendgrid-dark-landing.png) · [Recording](sendgrid-dark-landing.mp4) | Tokyo Night | 469 px | 456 px | 13 px up |
| [SendGrid, light](sendgrid-light-landing.png) · [Recording](sendgrid-light-landing.mp4) | Catppuccin Latte | 469 px | 456 px | 13 px up |

The article fixture uses eight paragraphs in a 600 px presentation table and a padded footer link. SendGrid keeps its mobile column layout at the pane's viewport width while the wider surface scales to fit. [493 px SendGrid](sendgrid-light-narrow.png) · [530 px article](article-dark-narrow.png)

## Calibration

Every sample stayed within 0–25 px of the painted height under both themes on private Xvfb and headless Sway. The table shows representative system-font measurements. The suite also runs with its bundled calibration font.

| Fixture | CSS viewport | Estimate | Painted | Estimate minus painted |
| --- | ---: | ---: | ---: | ---: |
| Fixed-width article | 493 px | 434.3 px | 422.3 px | +12.0 px |
| Fixed-width article | 530 px | 466.0 px | 454.0 px | +12.0 px |
| Fixed-width article | 960 px | 526.0 px | 514.0 px | +12.0 px |
| SendGrid | 493 px | 226.6 px | 213.6 px | +12.9 px |
| SendGrid | 530 px | 242.7 px | 229.7 px | +13.0 px |
| SendGrid | 960 px | 272.0 px | 258.0 px | +14.0 px |
| Mobile reveal in a 490 px window | 432 px | 334.0 px | 322.0 px | +12.0 px |
| Retail in a 960 px window | 530 px | 1806.7 px | 1800.0 px | +6.7 px |
| Grouped ESP reset | 530 px | 365.3 px | 353.3 px | +12.0 px |

## Keyboard flows

- Pressing `w` on a simple bulk message in Tokyo Night renders the white author background with black default text. The gray author text and blue default link also meet WCAG AA against their painted backgrounds. [Screenshot](author-colors-dark.png) · [Recording](author-colors-dark.mp4) · [Light](author-colors-light.png) · [Narrow](author-colors-narrow.png)
- Pressing `i` switches the personal message to web view. Its text starts inside a 14 px horizontal and 12 px vertical inset. The sample image uses a reserved fixture address. [Screenshot](personal-dark.png) · [Recording](personal-dark.mp4) · [Light](personal-light.png) · [Narrow](personal-narrow.png)
- Pressing `w` at the continuation row places Section 86 at the top of the visible reader below the subject. [Before](continuation-row.png) · [After](continuation-landed.png) · [Recording](continuation.mp4) · [Light](continuation-light.png) · [Narrow](continuation-narrow.png)
