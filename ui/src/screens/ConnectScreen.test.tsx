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

import { act, fireEvent, render, screen } from "@testing-library/react";
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
import { commands } from "../bindings";

/** The poll cadence `useServerStatus` runs at (15 s). */
const POLL_MS = 15_000;

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

function down() {
  return ok({
    state: "down",
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

  it("shows a refusal that arrives on a LATER poll, not only one it mounted with", async () => {
    // Mounted on `down` — no reason to show yet.
    statusMock.mockImplementationOnce(() => Promise.resolve(down()));
    statusMock.mockImplementation(() =>
      Promise.resolve(
        ok({
          state: "refused",
          binary_version: null,
          engine_fingerprint: null,
          reason: "the server refused this token (HTTP 401 Unauthorized)",
        }),
      ),
    );

    vi.useFakeTimers();
    try {
      render(<App />);
      await act(async () => {});
      expect(screen.getByRole("heading", { name: "Connect to the server" })).toBeTruthy();
      expect(screen.queryByRole("alert")).toBeNull();

      // The poll flips the still-mounted screen to `refused`: the reason has to
      // follow the prop, not the state it was seeded with on mount.
      await act(async () => {
        vi.advanceTimersByTime(POLL_MS);
      });
      const alert = screen.getByRole("alert");
      expect(alert.textContent).toContain("the server refused this token");
    } finally {
      vi.useRealTimers();
    }
  });

  it("trims the URL and the token before server_connect", async () => {
    statusMock.mockResolvedValue(notConnected());
    vi.mocked(commands.serverConnect).mockResolvedValue({
      status: "ok",
      data: { outcome: "connected", binary_version: "0.1.0", engine_fingerprint: "f0db" },
    });

    render(<App />);
    await screen.findByRole("heading", { name: "Connect to the server" });
    fireEvent.change(screen.getByLabelText("Server URL"), {
      target: { value: "  http://draco-desk:17620  " },
    });
    fireEvent.change(screen.getByLabelText("Token"), {
      target: { value: "  pt_pasted_with_whitespace  " },
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Connect" }));
    });

    // A pasted token carrying whitespace would otherwise be hashed padded (a
    // 401 that reads like a wrong token).
    expect(commands.serverConnect).toHaveBeenCalledWith(
      "http://draco-desk:17620",
      "pt_pasted_with_whitespace",
    );
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
