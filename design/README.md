# Design mockups

Static HTML, no build step: open `index.html` in a browser. The theme
picker (top right) switches between the five themes and persists.

- `tokens.css` is the contract: the semantic token vocabulary from
  `docs/UI.md`, five themes as five token blocks. When the design
  settles, these values transcribe into gpui-component theme JSON
  token for token; the mockups and the app share the vocabulary, not
  the code.
- `design.css` holds the shared components (window, sidebar, tabs,
  grid, inspector, bars, badges). Screens are one HTML file each.

These are direction, not pixel law: the GPUI build follows the
decisions (hierarchy, density, token roles, the LIVE and conflict
treatments), not the CSS.
