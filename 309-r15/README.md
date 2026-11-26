# Round 15 reader evidence

The recordings use the sample inbox, a private Weston compositor on Xvfb, temporary XDG directories, a private D-Bus, and a network namespace with only loopback. Tokyo Night and Catppuccin Latte use 960 × 780 px windows. The narrow captures use 530 × 780 px. The image-consent recordings include the expected unavailable-image feedback for the synthetic remote host.

The personal comparisons show simple view on the left and web view on the right. The link-focus images show the keyboard target over the original text. Tab followed by `y` copied the same demo destination in each theme.

The 8180 px table recordings show a case that the estimator recognizes at the pane width. The `calc()` recordings show a case discovered by WebKit. The dark montage samples frames 84, 95, 107 and 153. The light montage samples frames 108, 148, 165 and 183. Both show the placeholder snippet and its replacement by the simple body.

[Frame measurements](frame-measurements.tsv) log the card bottom and the sum and count of text pixels whose grayscale luminance differs from the nearby card fill. The text region excludes the prompt row and card border. The capture timestamps use the recording’s 60 fps timeline. Software rendering may repeat frames, so the recordings show transition continuity and geometry.

The personal card bottom stayed at y=308 in all 908 frames across both themes. Dark body ink stayed within 0.4% of the dimmer settled renderer or above it. Light body ink stayed at or above its baseline. Overlapping ink briefly brightened during the fade.

The predictable fallback kept the bottom at y=594 in all 246 frames. In the calculated-width recordings, every frame from the first visible placeholder snippet onward retained body ink. Both fallbacks settled in simple view without a retry row.
