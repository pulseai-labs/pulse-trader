// The real frontend entry point (r1.s1.w5), replacing the placeholder
// `r1.s1.w1` built to be thrown away. Mounts the ported design system's
// `App` (chrome + sidebar + the no-credential banner) into `#root`.
//
// `tokens.css` is imported before `shared.css`, matching the load order
// `docs/design/mock/README.md` documents ("Every screen links tokens.css
// then shared.css first"); `app-shell.css` and `content.css` are this app's
// own additions (not from the mock -- see each file's own header). `content.css`
// (r1.s1.w6) loads after `shared.css` so its `.content`/`.unbuilt` rules can
// rely on `shared.css`'s `.layout` grid already being defined. `library.css`
// (r1.s1.w3) is the Strategy Library's screen sheet and `designer.css`
// (r1.s1.w4) the Designer's — appended in load order, each after the shell
// sheets it builds on. `backtest.css` (r1.s3.w4) is the Backtest Lab's,
// carrying its scoped chart palette. `coach.css` (r1.s4.w3) is the coach rail's
// own sheet, loaded after `backtest.css` because the rail renders INSIDE the
// Lab's result column and builds on its section rules. `paper.css` (r3.s4.w5)
// is the promote sheet's, the badges', the Deployment Dashboard's and the
// session detail's — loaded last because its sheet and drawer render above
// every screen.

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App";
import "./styles/tokens.css";
import "./styles/shared.css";
import "./styles/app-shell.css";
import "./styles/content.css";
import "./styles/library.css";
import "./styles/designer.css";
import "./styles/backtest.css";
import "./styles/coach.css";
import "./styles/paper.css";

function element(id: string): HTMLElement {
  const found = document.getElementById(id);
  if (found === null) {
    throw new Error(`index.html is missing #${id}`);
  }
  return found;
}

createRoot(element("root")).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
