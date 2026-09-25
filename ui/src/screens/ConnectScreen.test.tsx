// The Connect gate's rendered tests (r3.s3.w5, AC-5/AC-6).
//
// `../bindings` is mocked exactly the way `LibraryScreen.test.tsx` mocks it —
// a `vi.mock` factory ahead of the mocked module's first import — because the
// real bindings talk to `window.__TAURI_INTERNALS__.invoke`, which has no
// jsdom stub. The three cases are the spec's own: the gate renders when not
// connected, a refusal names its reason, and the titlebar strip reads the
// polled status.
//
// `App` is rendered (not `ConnectScreen` alone) so the GATE decision — what
// mounts instead of the app — is itself under test.

import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const statusMock = vi.fn();

vi.mock("../bindings", () => ({
  commands: {
    serverStatus: () => statusMock(),
    serverConnect: vi.fn(),
    // The credential banner rides App; a no-credential env keeps the tree
    // quiet so the gate is the only thing under assertion.
    credentialStatus: vi.fn().mockResolvedValue("env"),
  },
}));

import { App } from "../App";

// The `typedError` union the real bindings resolve to, with the wire's
// snake_case status fields.
function ok(status: {
  state: string;
  binary_version: string | null;
  engine_fingerprint: string | null;
  reason: string | null;
}) {
  return { status: "ok", data: status };
}

function notConnected() {
  return ok({
    state: "not_connected",
    binary_version: null,
    engine_fingerprint: null,
    reason: null,
  });
}

describe("the Connect gate", () => {
  beforeEach(() => {
    statusMock.mockReset();
  });

  it("renders the Connect screen in place of the app when not connected", async () => {
    statusMock.mockResolvedValue(notConnected());
    const { container } = render(<App />);
    await screen.findByRole("heading", { name: "Connect to the server" });
    // The app's own nav must NOT be under the gate.
    expect(container.querySelector(".sidebar")).toBeNull();
    expect(screen.getByLabelText("Server URL")).toBeTruthy();
    expect(screen.getByLabelText("Token")).toBeTruthy();
  });

  it("shows the named refusal reason the status carries", async () => {
    statusMock.mockResolvedValue(
      ok({
        state: "refused",
        binary_version: null,
        engine_fingerprint: null,
        reason: "the server refused this token (HTTP 401 Unauthorized)",
      }),
    );
    render(<App />);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("the server refused this token");
  });

  it("reads the polled status into the titlebar strip when up", async () => {
    statusMock.mockResolvedValue(
      ok({
        state: "up",
        binary_version: "0.1.0",
        engine_fingerprint: "f0dbdf5748d38d5c",
        reason: null,
      }),
    );
    const { container } = render(<App />);
    // The gate is gone — the app's own sidebar is back.
    await screen.findByText(/server up/i, {}, { timeout: 3000 });
    expect(container.querySelector(".sidebar")).not.toBeNull();
    const strip = await screen.findByText(/server up/i, {}, { timeout: 3000 });
    expect(strip.textContent).toContain("0.1.0");
    expect(screen.queryByRole("heading", { name: "Connect to the server" })).toBeNull();
  });
});
