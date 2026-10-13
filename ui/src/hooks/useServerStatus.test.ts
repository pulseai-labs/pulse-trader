// Focused tests for the polled server-status hook (r4.s5.w4): the ordering
// guard of #389 and the last-good-read clock of #338.
//
// `../bindings` is mocked (the rendered tests' precedent). The hook's 15 s
// poll is driven with fake timers where a test needs a second tick; the
// ordering case drives two overlapping reads through promises the test
// settles in a chosen order — which is exactly the race #389 names.

import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const serverStatusMock = vi.fn();

vi.mock("../bindings", () => ({
  commands: {
    serverStatus: (...args: unknown[]) => serverStatusMock(...args),
  },
}));

import type { ServerStatus } from "../bindings";
import { useServerStatus } from "./useServerStatus";

/** The poll cadence `useServerStatus` runs at (15 s). */
const POLL_MS = 15_000;

function status(state: ServerStatus["state"], over: Partial<ServerStatus> = {}): ServerStatus {
  return {
    state,
    binary_version: null,
    engine_fingerprint: null,
    reason: null,
    role: null,
    ...over,
  };
}

function ok<T>(data: T) {
  return { status: "ok" as const, data };
}

/** A promise this test settles by hand. The executor form, not
 * `Promise.withResolvers` — this tree's `lib` is ES2022 (tsconfig.json), and
 * the typecheck stage is part of the gate. */
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((settle) => {
    resolve = settle;
  });
  return { promise, resolve };
}

beforeEach(() => {
  vi.clearAllMocks();
  serverStatusMock.mockResolvedValue(ok(status("up", { binary_version: "0.3.0" })));
});

afterEach(() => {
  vi.useRealTimers();
});

describe("useServerStatus", () => {
  it("applies only the newest issued answer, dropping an older late one (#389)", async () => {
    const older = deferred<{ status: "ok"; data: ServerStatus }>();
    const newer = deferred<{ status: "ok"; data: ServerStatus }>();
    serverStatusMock
      .mockImplementationOnce(() => older.promise)
      .mockImplementationOnce(() => newer.promise);

    const { result } = renderHook(() => useServerStatus());
    // The mount read (1) is still in flight; ask for a fresh read (2) — the
    // overlap a Connect/Disconnect refresh creates.
    act(() => {
      result.current.refresh();
    });
    expect(serverStatusMock).toHaveBeenCalledTimes(2);

    // The newer answer lands first...
    await act(async () => {
      newer.resolve(ok(status("down")));
    });
    await waitFor(() => expect(result.current.status?.state).toBe("down"));

    // ...and the OLDER one, answering last, must not overwrite it.
    await act(async () => {
      older.resolve(ok(status("up", { binary_version: "0.3.0" })));
    });
    expect(result.current.status?.state).toBe("down");
  });

  it("applies a stalled read's answer while no newer answer has applied (#389)", async () => {
    const stalled = deferred<{ status: "ok"; data: ServerStatus }>();
    const next = deferred<{ status: "ok"; data: ServerStatus }>();
    serverStatusMock
      .mockImplementationOnce(() => stalled.promise)
      .mockImplementationOnce(() => next.promise);

    const { result } = renderHook(() => useServerStatus());
    // The mount read (1) stalls; the next poll (2) is issued and is still out.
    act(() => {
      result.current.refresh();
    });
    expect(serverStatusMock).toHaveBeenCalledTimes(2);

    // Read 1 answers `down` with nothing newer applied: it must land — the
    // gate cannot stay blank because the next read was merely ISSUED.
    await act(async () => {
      stalled.resolve(ok(status("down")));
    });
    expect(result.current.status?.state).toBe("down");

    // And the newer read, when it answers, still wins over the older answer.
    await act(async () => {
      next.resolve(ok(status("up", { binary_version: "0.3.0" })));
    });
    expect(result.current.status?.state).toBe("up");
  });

  it("records the last read that answered up, and only that read (#338)", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-10-13T18:15:45Z"));

    const { result } = renderHook(() => useServerStatus());
    await act(async () => {});
    expect(result.current.status?.state).toBe("up");
    expect(result.current.lastGoodAt).toBe(Date.UTC(2026, 9, 13, 18, 15, 45));

    // A poll that answers `down` (or fails) is not a good read: the time stays
    // the last time the server was actually seen.
    vi.setSystemTime(new Date("2026-10-13T18:16:00Z"));
    serverStatusMock.mockResolvedValue(ok(status("down")));
    await act(async () => {
      vi.advanceTimersByTime(POLL_MS);
    });
    expect(result.current.status?.state).toBe("down");
    expect(result.current.lastGoodAt).toBe(Date.UTC(2026, 9, 13, 18, 15, 45));

    // A later `up` moves it again — the poll above fired 15 s after the set
    // time, so this one answers at 18:16:30.
    serverStatusMock.mockResolvedValue(ok(status("up", { binary_version: "0.3.0" })));
    await act(async () => {
      vi.advanceTimersByTime(POLL_MS);
    });
    expect(result.current.lastGoodAt).toBe(Date.UTC(2026, 9, 13, 18, 16, 30));
  });

  it("keeps the last status when a read itself fails", async () => {
    const { result } = renderHook(() => useServerStatus());
    await waitFor(() => expect(result.current.status?.state).toBe("up"));

    serverStatusMock.mockRejectedValue(new Error("bridge down"));
    await act(async () => {
      result.current.refresh();
    });
    expect(result.current.status?.state).toBe("up");
  });
});
