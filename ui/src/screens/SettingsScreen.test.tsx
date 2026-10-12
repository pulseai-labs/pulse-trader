// The Settings screen's rendered tests (r4.s3.w3, spec §Approach 2 and AC-1).
//
// `../bindings` is mocked the way `ConnectScreen.test.tsx` mocks it. `App` is
// rendered with `#/settings` so the ROUTES entry, the shared connection context
// and the screen itself are all under test together — a screen mounted without
// its provider would throw, which is the point.
//
// The cases are the spec's own: the state from the polled status, the address
// from `server_address`, "Change server" opening Connect, and Disconnect's
// confirm step and its result — including the amended rule that a confirmed
// Disconnect shows the disconnected state and the revoke sentence at once,
// without waiting for the next poll.

import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const statusMock = vi.fn();

vi.mock("../bindings", () => ({
  commands: {
    serverStatus: () => statusMock(),
    serverConnect: vi.fn(),
    serverAddress: vi.fn().mockResolvedValue({ status: "ok", data: null }),
    serverDisconnect: vi.fn().mockResolvedValue({ status: "ok", data: null }),
    credentialStatus: vi.fn().mockResolvedValue("env"),
    paperSessions: vi.fn().mockResolvedValue({ status: "ok", data: [] }),
  },
}));

import { App } from "../App";
import { commands } from "../bindings";
import type { ServerStatus } from "../bindings";

/** The stored connection's address the command returns in these tests. */
const ADDRESS = "http://100.90.203.12:8420";

/** The poll cadence `useServerStatus` runs at (15 s). */
const POLL_MS = 15_000;

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

/** The section's state chip, by its `data-state`. */
function stateChip(): HTMLElement {
  const chip = document.querySelector(".srv-card [data-state]");
  if (chip === null) {
    throw new Error("no state chip rendered");
  }
  return chip as HTMLElement;
}

/** Open Settings and wait for the section to exist. */
async function openSettings() {
  render(<App />);
  await screen.findByRole("heading", { name: "Server connection" });
}

describe("the Settings server-connection section (r4.s3.w3)", () => {
  beforeEach(() => {
    window.location.hash = "#/settings";
    statusMock.mockReset();
    statusMock.mockResolvedValue(
      okStatus({
        state: "up",
        binary_version: "0.1.0",
        engine_fingerprint: "f0dbdf5748d38d5c",
      }),
    );
    vi.mocked(commands.serverAddress).mockReset();
    vi.mocked(commands.serverAddress).mockResolvedValue({ status: "ok", data: ADDRESS });
    vi.mocked(commands.serverDisconnect).mockReset();
    vi.mocked(commands.serverDisconnect).mockResolvedValue({ status: "ok", data: null });
  });

  afterEach(() => {
    window.location.hash = "";
  });

  it("shows the connected state, the address server_address returned, and the wire facts", async () => {
    await openSettings();

    expect(stateChip().getAttribute("data-state")).toBe("connected");
    expect(stateChip().textContent).toContain("connected");
    // The situated fact: the address comes from the command, never from a form.
    expect(await screen.findByText(ADDRESS)).toBeTruthy();
    expect(screen.getByText("pulse 0.1.0")).toBeTruthy();
    expect(screen.getByText("f0dbdf5748d38d5c")).toBeTruthy();
    expect(screen.getByText("The token itself is never shown after connecting.")).toBeTruthy();
    // No token field is on this screen at all.
    expect(screen.queryByLabelText(/app token/i)).toBeNull();

    // And the section is the only settings content: the mock's other sections
    // are out of scope and render nothing.
    expect(screen.queryByText(/AI provider/i)).toBeNull();
  });

  it("shows reconnecting while the server is not answering", async () => {
    vi.useFakeTimers();
    try {
      render(<App />);
      await act(async () => {});
      // Mounted while the server answers: the app is open, Settings is up.
      expect(
        screen.getByRole("heading", { name: "Server connection" }),
      ).toBeTruthy();
      expect(stateChip().getAttribute("data-state")).toBe("connected");

      // The server stops answering. The app holds (today's tolerance), and the
      // section reports the status that actually arrived — `reconnecting`.
      statusMock.mockResolvedValue(okStatus({ state: "down" }));
      await act(async () => {
        vi.advanceTimersByTime(POLL_MS);
      });

      expect(document.querySelector(".sidebar")).not.toBeNull();
      expect(stateChip().getAttribute("data-state")).toBe("reconnecting");
      expect(stateChip().textContent).toContain("reconnecting");
      // Nothing is invented while the server is silent; the stored address stays.
      expect(screen.getByText(ADDRESS)).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });

  it("asks to confirm before disconnecting, and Cancel calls nothing", async () => {
    await openSettings();

    fireEvent.click(screen.getByRole("button", { name: "Disconnect" }));
    expect(screen.getByText("Forget the token on this Mac?")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByText("Forget the token on this Mac?")).toBeNull();
    expect(commands.serverDisconnect).not.toHaveBeenCalled();
  });

  it("shows the disconnected state and the revoke sentence straight after a confirmed Disconnect", async () => {
    await openSettings();

    // The poll keeps answering `up` throughout: the screen's new state cannot
    // come from it.
    fireEvent.click(screen.getByRole("button", { name: "Disconnect" }));
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Forget the token" }));
    });
    await waitFor(() => {
      expect(commands.serverDisconnect).toHaveBeenCalledTimes(1);
    });

    expect(stateChip().getAttribute("data-state")).toBe("disconnected");
    const note = screen.getByText(/This Mac no longer holds the token/);
    expect(note.textContent).toContain("pulse token revoke <label>");
    expect(statusMock).toHaveBeenCalled();
    // The pin: the last status that arrived is still `up`, and the screen does
    // not wait for — or take its state from — anything newer.
    const last = await statusMock.mock.results.at(-1)?.value;
    expect(last.data.state).toBe("up");
  });

  it("offers the way to Connect from the disconnected section, and Back returns", async () => {
    await openSettings();

    fireEvent.click(screen.getByRole("button", { name: "Change server" }));
    // "Change server" opens the full-window Connect screen over the live app.
    expect(screen.getByRole("heading", { name: "Connect to your Pulse server" })).toBeTruthy();
    expect(document.querySelector(".sidebar")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Back to the app" }));
    expect(screen.getByRole("heading", { name: "Server connection" })).toBeTruthy();
    expect(document.querySelector(".sidebar")).not.toBeNull();
  });
});
