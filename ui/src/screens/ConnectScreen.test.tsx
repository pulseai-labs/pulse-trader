// The Connect screen's rendered tests (r4.s3.w3, spec §Approach 1 and AC-1).
//
// `../bindings` is mocked exactly the way `LibraryScreen.test.tsx` mocks it — a
// `vi.mock` factory ahead of the mocked module's first import — because the real
// bindings talk to `window.__TAURI_INTERNALS__.invoke`, which has no jsdom stub.
//
// `App` is rendered (not `ConnectScreen` alone) so the GATE decision — what mounts
// instead of the app — is itself under test, and so the screen's `status` prop is
// the real polled one.
//
// The cases are the spec's own: every state the mock names, the step marking that
// comes from the ONE `server_connect` outcome and nothing else, the reveal toggle,
// the recovery command, and the one storage sentence.

import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const statusMock = vi.fn();

vi.mock("../bindings", () => ({
  commands: {
    serverStatus: () => statusMock(),
    serverConnect: vi.fn(),
    // r4.s3.w3: the stored connection's URL — the idle form prefills from it and
    // the connected card shows it. `null` is "no connection stored".
    serverAddress: vi.fn().mockResolvedValue({ status: "ok", data: null }),
    serverDisconnect: vi.fn().mockResolvedValue({ status: "ok", data: null }),
    // The credential banner rides App; a no-credential env keeps the tree quiet.
    credentialStatus: vi.fn().mockResolvedValue("env"),
    // r3.s4.w5: App polls the running paper-session count for the titlebar pill.
    paperSessions: vi.fn().mockResolvedValue({ status: "ok", data: [] }),
  },
}));

import { App } from "../App";
import { commands } from "../bindings";
import type { ConnectOutcome, ServerStatus } from "../bindings";

/** The poll cadence `useServerStatus` runs at (15 s). */
const POLL_MS = 15_000;

type OutcomeAnswer = { status: "ok"; data: ConnectOutcome };

/** The `typedError` union the real bindings resolve to, with snake_case fields. */
function okStatus(status: Partial<ServerStatus> & { state: ServerStatus["state"] }): {
  status: "ok";
  data: ServerStatus;
} {
  return {
    status: "ok",
    data: {
      binary_version: null,
      engine_fingerprint: null,
      reason: null,
      role: null,
      ...status,
    },
  };
}

const NOT_CONNECTED = {
  state: "not_connected",
  binary_version: null,
  engine_fingerprint: null,
  reason: null,
  role: null,
} as const;

function refused(reason: string) {
  return okStatus({ state: "refused", reason });
}

/** Submit the form with a URL and a token. */
async function submit(url: string, token: string) {
  fireEvent.change(screen.getByLabelText(/server url/i), { target: { value: url } });
  fireEvent.change(screen.getByLabelText(/app token/i), { target: { value: token } });
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: /^(connect|connecting)/i }));
  });
}

/** One step row's `data-state`, by step id. */
function stepState(step: string): string | null {
  return document.querySelector(`[data-step="${step}"]`)?.getAttribute("data-state") ?? null;
}

describe("the Connect screen's states (r4.s3.w3)", () => {
  beforeEach(() => {
    statusMock.mockReset();
    statusMock.mockResolvedValue(okStatus(NOT_CONNECTED));
    vi.mocked(commands.serverConnect).mockReset();
    vi.mocked(commands.serverAddress).mockReset();
    vi.mocked(commands.serverAddress).mockResolvedValue({ status: "ok", data: null });
    vi.mocked(commands.serverDisconnect).mockReset();
  });

  it("renders the idle form in place of the app when not connected", async () => {
    const { container } = render(<App />);
    await screen.findByRole("heading", { name: "Connect to your Pulse server" });
    // The app's own nav must NOT be under the gate.
    expect(container.querySelector(".sidebar")).toBeNull();
    expect(screen.getByLabelText(/server url/i)).toBeTruthy();
    expect(screen.getByLabelText(/app token/i)).toBeTruthy();
    // r4.s3 fix round (C2): the hint is the ONE prefix every token carries —
    // the server issues `pt_…` for both scopes (src/server/auth.rs).
    expect((screen.getByLabelText(/app token/i) as HTMLInputElement).placeholder).toBe("pt_…");
    // The help block carries the command a trader runs on the server, and the
    // storage sentence states the local footprint in one line.
    expect(screen.getByText("pulse token issue --scope app --label <name>")).toBeTruthy();
    expect(
      screen.getByText(
        "Stored on this Mac: the server address, and a revocable app token in the macOS Keychain. Nothing else.",
      ),
    ).toBeTruthy();
  });

  it("prefills the server URL with the stored address", async () => {
    vi.mocked(commands.serverAddress).mockResolvedValue({
      status: "ok",
      data: "http://100.90.203.12:8420",
    });
    render(<App />);
    // The read resolves after the mount; the field follows it.
    const url = await screen.findByDisplayValue("http://100.90.203.12:8420");
    expect((url as HTMLInputElement).id).toBe("connect-url");
  });

  it("reveals and hides the token field without ever rendering the token value elsewhere", async () => {
    render(<App />);
    await screen.findByRole("heading", { name: "Connect to your Pulse server" });
    const field = screen.getByLabelText(/app token/i) as HTMLInputElement;
    expect(field.type).toBe("password");

    fireEvent.change(field, { target: { value: "pt_app_test_token" } });
    // The toggle appears only once there is something to reveal.
    fireEvent.click(screen.getByRole("button", { name: "Reveal" }));
    expect((screen.getByLabelText(/app token/i) as HTMLInputElement).type).toBe("text");

    fireEvent.click(screen.getByRole("button", { name: "Hide" }));
    expect((screen.getByLabelText(/app token/i) as HTMLInputElement).type).toBe("password");
  });

  it("shows the three steps pending while the one server_connect call is in flight", async () => {
    let settle!: (answer: OutcomeAnswer) => void;
    const pending = new Promise<OutcomeAnswer>((resolve) => {
      settle = resolve;
    });
    vi.mocked(commands.serverConnect).mockImplementation(() => pending);

    render(<App />);
    await screen.findByRole("heading", { name: "Connect to your Pulse server" });
    await submit("http://100.90.203.12:8420", "pt_app_test_token");

    // No timer advances a step: while the call is in flight every row is pending.
    expect(document.querySelectorAll("[data-step]")).toHaveLength(3);
    for (const step of ["reach", "token", "versions"]) {
      expect(stepState(step)).toBe("pending");
    }
    expect(screen.getByText(/reach http:\/\/100\.90\.203\.12:8420/i)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Connecting…" })).toBeTruthy();

    // Settle it so the test leaves no dangling promise.
    await act(async () => {
      settle({ status: "ok", data: { outcome: "token_refused", reason: "test settle" } });
      await pending;
    });
  });

  it("marks the steps from a token_refused outcome: reach done, token failed, versions pending", async () => {
    vi.mocked(commands.serverConnect).mockResolvedValue({
      status: "ok",
      data: { outcome: "token_refused", reason: "the server refused this token (HTTP 401)" },
    });
    render(<App />);
    await screen.findByRole("heading", { name: "Connect to your Pulse server" });
    await submit("http://100.90.203.12:8420", "pt_app_bad");

    expect(stepState("reach")).toBe("done");
    expect(stepState("token")).toBe("failed");
    expect(stepState("versions")).toBe("pending");
    const alert = screen.getByRole("alert");
    expect(alert.textContent).toContain("The server refused this token");
    expect(alert.textContent).toContain("the server refused this token (HTTP 401)");
    // The recovery command is copyable right where the failure is explained.
    expect(
      within(alert).getByText("pulse token issue --scope app --label <name>"),
    ).toBeTruthy();
  });

  it("marks the steps from an unreachable outcome: reach failed, the rest pending", async () => {
    vi.mocked(commands.serverConnect).mockResolvedValue({
      status: "ok",
      data: { outcome: "unreachable", reason: "handshake failed: connection refused" },
    });
    render(<App />);
    await screen.findByRole("heading", { name: "Connect to your Pulse server" });
    await submit("http://100.90.203.12:8420", "pt_app_test_token");

    expect(stepState("reach")).toBe("failed");
    expect(stepState("token")).toBe("pending");
    expect(stepState("versions")).toBe("pending");
    const alert = screen.getByRole("alert");
    expect(alert.textContent).toContain("Can't reach 100.90.203.12:8420");
    expect(alert.textContent).toContain("Tailscale");
    expect(screen.getByText("tailscale ping 100.90.203.12")).toBeTruthy();
  });

  it("marks the steps from a skew outcome: versions failed, both sides side by side", async () => {
    vi.mocked(commands.serverConnect).mockResolvedValue({
      status: "ok",
      data: { outcome: "skew", server_api_version: 2 },
    });
    render(<App />);
    await screen.findByRole("heading", { name: "Connect to your Pulse server" });
    await submit("http://100.90.203.12:8420", "pt_app_test_token");

    expect(stepState("reach")).toBe("done");
    expect(stepState("token")).toBe("done");
    expect(stepState("versions")).toBe("failed");
    const alert = screen.getByRole("alert");
    expect(alert.textContent).toContain("The server runs a different version");
    expect(alert.textContent).toContain("API v2");
    expect(alert.textContent).toContain("API v1");
  });

  it("renders the connected card from the handshake's own facts and opens the app", async () => {
    vi.mocked(commands.serverAddress).mockResolvedValue({
      status: "ok",
      data: "http://100.90.203.12:8420",
    });
    vi.mocked(commands.serverConnect).mockResolvedValue({
      status: "ok",
      data: {
        outcome: "connected",
        binary_version: "0.1.0",
        engine_fingerprint: "f0dbdf5748d38d5c",
      },
    });
    // The poll that follows the card answers `up`, so "Open the app" flips the gate.
    // (Set AFTER the mount poll, which must answer not_connected so the gate shows
    // this screen at all.)
    const { container } = render(<App />);
    await screen.findByRole("heading", { name: "Connect to your Pulse server" });
    await submit("http://100.90.203.12:8420", "pt_app_test_token");

    expect(screen.getByRole("heading", { name: "Connected to your Pulse server" })).toBeTruthy();
    expect(screen.getByText("pulse 0.1.0")).toBeTruthy();
    expect(screen.getByText("f0dbdf5748d38d5c")).toBeTruthy();
    expect(screen.getByText("http://100.90.203.12:8420")).toBeTruthy();
    expect(screen.getByText(/opening in \d s/)).toBeTruthy();

    statusMock.mockResolvedValue(
      okStatus({
        state: "up",
        binary_version: "0.1.0",
        engine_fingerprint: "f0dbdf5748d38d5c",
      }),
    );
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: /open the app/i }));
    });
    // The gate lifts: the app's own shell is back.
    expect(container.querySelector(".sidebar")).not.toBeNull();
  });

  it("recognises an agent token from the server's scope refusal", async () => {
    vi.mocked(commands.serverConnect).mockResolvedValue({
      status: "ok",
      data: {
        outcome: "token_refused",
        reason:
          "the presented token is agent-scoped and this connection needs an app-scoped token " +
          "(ask the server operator for a token issued with `--scope app`)",
      },
    });
    render(<App />);
    await screen.findByRole("heading", { name: "Connect to your Pulse server" });
    await submit("http://100.90.203.12:8420", "pt_agt_test_token");

    const alert = screen.getByRole("alert");
    expect(alert.textContent).toContain("This is an agent token, for MCP clients");
    expect(alert.textContent).toContain("agent-scoped");
    // r4.s3 fix round (C2): no scope-specific prefix is claimed — both scopes
    // carry the one `pt_` prefix (`src/server/auth.rs`'s TOKEN_PREFIX).
    expect(alert.textContent).toContain("pt_…");
    expect(alert.textContent).not.toContain("pt_agt_");
    expect(alert.textContent).not.toContain("pt_app_");
  });

  it("recognises a refusal that arrives on a LATER poll, not only one it mounted with", async () => {
    vi.useFakeTimers();
    try {
      // Mounted on `not_connected` — no reason to show yet.
      statusMock.mockResolvedValue(okStatus(NOT_CONNECTED));
      render(<App />);
      await act(async () => {});
      expect(
        screen.getByRole("heading", { name: "Connect to your Pulse server" }),
      ).toBeTruthy();
      expect(screen.queryByRole("alert")).toBeNull();

      // The poll flips the still-mounted screen to `refused`: the reason has to
      // follow the prop, not the state it was seeded with on mount.
      statusMock.mockResolvedValue(
        refused("the server refused this token (HTTP 401 Unauthorized)"),
      );
      await act(async () => {
        vi.advanceTimersByTime(POLL_MS);
      });
      const alert = screen.getByRole("alert");
      expect(alert.textContent).toContain("the server refused this token");
      expect(
        screen.getByRole("heading", { name: "Reconnect to your Pulse server" }),
      ).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });

  it("trims the URL and the token before server_connect", async () => {
    vi.mocked(commands.serverConnect).mockResolvedValue({
      status: "ok",
      data: {
        outcome: "unreachable",
        reason: "handshake failed: connection refused",
      },
    });

    render(<App />);
    await screen.findByRole("heading", { name: "Connect to your Pulse server" });
    await submit("  http://draco-desk:17620  ", "  pt_pasted_with_whitespace  ");

    // A pasted token carrying whitespace would otherwise be hashed padded (a 401
    // that reads like a wrong token).
    expect(commands.serverConnect).toHaveBeenCalledWith(
      "http://draco-desk:17620",
      "pt_pasted_with_whitespace",
    );
  });

  it("reads the polled status into the titlebar strip when up", async () => {
    statusMock.mockResolvedValue(
      okStatus({
        state: "up",
        binary_version: "0.1.0",
        engine_fingerprint: "f0dbdf5748d38d5c",
      }),
    );
    const { container } = render(<App />);
    // The gate is gone — the app's own sidebar is back.
    await screen.findByText(/server up/i, {}, { timeout: 3000 });
    expect(container.querySelector(".sidebar")).not.toBeNull();
    const strip = await screen.findByText(/server up/i, {}, { timeout: 3000 });
    expect(strip.textContent).toContain("0.1.0");
    expect(screen.queryByRole("heading", { name: "Connect to your Pulse server" })).toBeNull();
  });

  it("shows the refusal the status carries, on the reconnect form", async () => {
    statusMock.mockResolvedValue(
      refused("the server refused this token (HTTP 401 Unauthorized)"),
    );
    render(<App />);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("the server refused this token");
    expect(
      screen.getByRole("heading", { name: "Reconnect to your Pulse server" }),
    ).toBeTruthy();
  });
});
