# Design mockups

Static HTML, no build step: open `index.html` in a browser. The theme
picker (top right) switches between the five themes and persists.

- `tokens.css` is the contract: the semantic token vocabulary from
  `../docs/UI.md`, five themes as five token blocks. When the design
  settles, these values transcribe into gpui-component theme JSON
  token for token; the mockups and the app share the vocabulary, not
  the code.
- `design.css` holds the shared components (window, sidebar, tabs,
  grid, inspector, bars, badges). Screens are one HTML file each.

These proofs are the visual arbiter: when a shipped surface and its
proof disagree, the proof wins, and the build cites the exact CSS it
follows (`design.css` `.grid`, `.seg`, `.bbar` sizes appear in code
comments). Deviations happen only where the toolkit forces them, and
the code comment at the site says so.
