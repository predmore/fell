# Contributing

## Review gates

UX quality is a top priority. Every PR needs CI and the code reviewer. A PR that changes something the user sees or touches (UI, keymap, rendering, interaction, performance feel) also needs the UX gate below. This applies from M1's inbox PR onward. The same rule is in [docs/scope.md](docs/scope.md). [docs/ux.md](docs/ux.md), together with [docs/scope.md](docs/scope.md), is the bar the UX judges score against.

Judges must be independent: they did not write the code. They need running-app evidence, not just the diff. Attach that evidence to the PR description:

- Screenshots of the running app. Before and after when changing existing UI. Several window sizes.
- A short recording for keyboard flows.
- For the inbox: screenshots of the running app on a dark stock Omarchy theme, a light stock Omarchy theme, and a narrow window, plus a keyboard recording.

Judges score those same images and the recording against [docs/ux.md](docs/ux.md) together with [docs/scope.md](docs/scope.md): Zed-like speed and polish, keyboard-first vim grammar, typography and density, clarity, focus states, empty and loading states, error handling. Each score is out of 10.

Iterate until every judge is at 9.5 or better. Put the scores and the evidence in the PR. Do not merge a visible change while any judge is below 9.5.
