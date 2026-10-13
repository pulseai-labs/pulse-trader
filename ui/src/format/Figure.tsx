// The one display component for a formatted figure (r4.s5.w1, A1): the
// formatted text, with the server's exact text one hover away as `title` and
// in the DOM as `data-exact` for the screen tests.
//
// Nothing else computes or formats a figure in this app: a second formatter
// would be a second answer to "what does this number read as", and the two
// would drift.

import type { ReactNode } from "react";

export function Figure({ exact, children }: { exact: string | null; children: ReactNode }) {
  return (
    <span title={exact ?? undefined} data-exact={exact ?? undefined}>
      {children}
    </span>
  );
}
