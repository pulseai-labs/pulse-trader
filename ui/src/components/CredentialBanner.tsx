// The no-credential banner (r1.s1.w5, grill G4/A7).
//
// A fresh install has an empty `pulse.db` and no LLM credential resolvable, so the
// first thing a first-run user would try -- composing a strategy -- fails mid-stream
// with no warning. G4 exists to design that state rather than leave it blank: this
// banner states the fact up front and names the next action.
//
// It reads `credential_status`, the command `r1.s1.w5` adds to wire `w2`'s
// value-free `CredentialStatus` read onto the bus. The wire type carries no key
// material at all (`src/domain/secret.rs`), so there is nothing here that could ever
// render a credential -- and the copy below deliberately names only the environment
// variable, never a filesystem path, so it cannot even hint at where a credential
// file would need to sit.
//
// r3.s3.w5 moved the resolution SERVER-side (`src/adapters/secrets.rs` resolves it
// for `pulse serve`), so the copy names the server host: telling a Mac-app user to
// set the variable in the app's own environment would send them to a place the
// server never reads.
//
// Non-blocking (G4's whole point): a user with no credential can still open and
// navigate the shell. This component renders a dismissible-by-navigation notice, not
// a gate -- nothing else in the frame is disabled while it is showing.

import { useEffect, useState } from "react";

import { commands } from "../bindings";
import type { CredentialStatus } from "../bindings";

export function CredentialBanner() {
  const [status, setStatus] = useState<CredentialStatus | null>(null);

  useEffect(() => {
    let cancelled = false;
    commands
      .credentialStatus()
      .then((result) => {
        if (cancelled) {
          return;
        }
        // r3.s3.w5: the read crossed the wire and grew the bus's `Result`
        // shell (`typedError`), so the answer is a discriminated union. `ok`
        // carries the status; `error` is a REAL failure (the server could not
        // be asked) -- the banner must not claim "no credential" for it, so it
        // renders nothing, exactly like the unreachable case below.
        if (result.status === "ok") {
          setStatus(result.data);
        } else {
          setStatus(null);
        }
      })
      .catch(() => {
        // A rejection here means the IPC call itself failed (e.g. no app
        // handle in a non-Tauri preview). Staying silent is correct: a banner
        // that cannot confirm there IS no credential must not claim there is
        // one, and must not block the shell.
        if (!cancelled) {
          setStatus(null);
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  if (status !== "none") {
    return null;
  }

  return (
    <div className="credential-banner" role="status">
      No LLM credential found on the server yet. Set the{" "}
      <code className="mono">OLLAMA_API_KEY</code> environment variable for the server —
      on the host that runs <code className="mono">pulse serve</code>, then restart it —
      to enable strategy composition. You can still browse the shell without one.
    </div>
  );
}
