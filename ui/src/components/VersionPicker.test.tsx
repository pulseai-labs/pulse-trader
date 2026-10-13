// The version picker's focused suite (r4.s5.w2, A4) — authored BEFORE the
// component exists and driven RED, mirroring `BacktestLabScreen.test.tsx`'s
// straight-DOM style: no command layer here at all, the component takes the
// catalog's own `LibraryStrategy[]` and reports selections through `onChange`.
//
// The fixture is the A4 shape at scale: 12 strategies × 10 versions = 120
// versions, two of the twelve sharing the #318 name ("M15 RSI Oversold H4 Trend
// Long", created 2026-09-20 and 2026-09-22) with different short ids. Version
// ids are `1aurora`…`10aurora` — the version number first, so every version's
// 8-character short id is distinct, the way a real UUID is and a shared
// strategy prefix would not be.
//
// Covers: filtering (name / `v<n>` / short id, case-insensitive), grouping in
// catalog order with versions in `version_tree` order, the duplicate-name
// header disambiguator (#318), the option text (version label, created date,
// short id, the Library's creator label, the pinned marker), the empty-filter
// line, disabled-while-busy, and the whole keyboard map (↓/↑ clamped, Enter,
// Escape, Home/End) with focus held in the input and `aria-activedescendant`
// tracking the active option.

import { fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";

import type { LibraryStrategy, LibraryVersion } from "../bindings";
import { VersionPicker } from "./VersionPicker";

// ---------------------------------------------------------------------------
// The 120-version fixture
// ---------------------------------------------------------------------------

/** The #318 name, on two strategies with different creation dates. */
const DUP_NAME = "M15 RSI Oversold H4 Trend Long";

function fixtureVersion(
  key: string,
  n: number,
  createdBy = "human",
  agentName: string | null = null,
): LibraryVersion {
  return {
    id: `${n}${key}`,
    parentId: n === 1 ? null : `${n - 1}${key}`,
    createdAt: `2026-08-${String(n).padStart(2, "0")}T10:00:00.000Z`,
    createdBy,
    agentName,
    hypothesis: null,
    dsl: {
      name: "Fixture",
      direction: "long",
      entry: ["rsi(14) < 30"],
      filters: [],
      exits: ["stop 5%", "take profit 2R"],
      risk: ["risk 1% per trade", "max leverage 3x"],
    },
    stats: null,
    deltaVsParent: null,
    recentRuns: [],
    latestRun: null,
    certified: false,
    latestWalkForwardRunId: null,
  };
}

/** One strategy with ten versions, `v1`…`v10` (ids `1<key>`…`10<key>`). */
function fixtureStrategy(
  key: string,
  name: string,
  createdAt: string,
  overrides: {
    pinnedVersionId?: string | null;
    createdBy?: string;
    agentName?: string | null;
  } = {},
): LibraryStrategy {
  const createdBy = overrides.createdBy ?? "human";
  const agentName = overrides.agentName ?? null;
  return {
    id: key,
    name,
    createdAt,
    pinnedVersionId: overrides.pinnedVersionId ?? null,
    versions: Array.from({ length: 10 }, (_, i) =>
      fixtureVersion(key, i + 1, createdBy, agentName),
    ),
  };
}

const STRATEGIES: LibraryStrategy[] = [
  fixtureStrategy("aurora", "Aurora Momentum", "2026-08-01T09:00:00.000Z"),
  fixtureStrategy("dupe20", DUP_NAME, "2026-09-20T09:00:00.000Z"),
  fixtureStrategy("dupe22", DUP_NAME, "2026-09-22T09:00:00.000Z"),
  fixtureStrategy("delta4", "Delta Reversion", "2026-08-03T09:00:00.000Z", {
    pinnedVersionId: "4delta4",
  }),
  fixtureStrategy("echo55", "Echo Breakout", "2026-08-04T09:00:00.000Z"),
  fixtureStrategy("foxtr6", "Foxtrot Fade", "2026-08-05T09:00:00.000Z"),
  fixtureStrategy("golf77", "Golf Trend", "2026-08-06T09:00:00.000Z", {
    createdBy: "external_agent",
    agentName: "seed-r4s5",
  }),
  fixtureStrategy("hotel8", "Hotel Pullback", "2026-08-07T09:00:00.000Z"),
  fixtureStrategy("india9", "India Squeeze", "2026-08-08T09:00:00.000Z"),
  fixtureStrategy("juliet", "Juliet Range", "2026-08-09T09:00:00.000Z"),
  fixtureStrategy("kilo11", "Kilo Divergence", "2026-08-10T09:00:00.000Z"),
  fixtureStrategy("lima22", "Lima Carry", "2026-08-11T09:00:00.000Z"),
];

const FIRST_VERSION = "1aurora";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** A controlled harness: a selection made through the UI updates the input's
 * displayed value the way the real screen's state does. */
function Harness({
  initial = FIRST_VERSION,
  onSelect,
}: {
  initial?: string;
  onSelect?: (versionId: string) => void;
}) {
  const [value, setValue] = useState<string>(initial);
  return (
    <VersionPicker
      strategies={STRATEGIES}
      value={value}
      onChange={(id) => {
        setValue(id);
        onSelect?.(id);
      }}
      disabled={false}
    />
  );
}

/** Focus the combobox the way the trader does — a real focus, so it is the
 * document's active element, plus the focus event React's handler listens for —
 * and hand it back for the steps that follow. */
function open(): HTMLInputElement {
  const box = screen.getByRole("combobox") as HTMLInputElement;
  box.focus();
  fireEvent.focus(box);
  return box;
}

/** The text of the option `aria-activedescendant` names, or `null` when no
 * option is active. */
function activeOptionText(): string | null {
  const id = (screen.getByRole("combobox") as HTMLInputElement).getAttribute(
    "aria-activedescendant",
  );
  if (id === null) {
    return null;
  }
  return document.getElementById(id)?.textContent ?? null;
}

/** Every option's text, in the order the popup shows them. */
function optionTexts(): string[] {
  return screen.getAllByRole("option").map((option) => option.textContent ?? "");
}

// ---------------------------------------------------------------------------
// Filtering
// ---------------------------------------------------------------------------

describe("VersionPicker (filtering)", () => {
  it("filters case-insensitively on the strategy name, the label and the short id", () => {
    render(<Harness />);
    const box = open();

    // Strategy name, case-insensitively: "delta reversion" and its uppercase
    // form both land on that one group's ten versions.
    fireEvent.change(box, { target: { value: "delta reversion" } });
    expect(screen.getAllByRole("option")).toHaveLength(10);
    fireEvent.change(box, { target: { value: "DELTA REVERSION" } });
    expect(screen.getAllByRole("option")).toHaveLength(10);

    // The version label: `v3` is one option in each of the twelve strategies.
    fireEvent.change(box, { target: { value: "v3" } });
    const atV3 = optionTexts();
    expect(atV3).toHaveLength(12);
    expect(atV3.every((text) => text.startsWith("v3 · "))).toBe(true);

    // The short id: exactly the one version whose first 8 characters match.
    fireEvent.change(box, { target: { value: "7KILO" } });
    const byShortId = screen.getAllByRole("option");
    expect(byShortId).toHaveLength(1);
    expect(byShortId[0]?.textContent).toContain("7kilo");
  });

  it("renders only the filtered list — a 120-version catalog narrows to the matches", () => {
    render(<Harness />);
    const box = open();

    // The unfiltered popup is the whole catalog…
    expect(screen.getAllByRole("option")).toHaveLength(120);

    // …and the matches are the only options rendered.
    fireEvent.change(box, { target: { value: "juliet range" } });
    expect(screen.getAllByRole("option")).toHaveLength(10);
  });

  it("says so in one line when nothing matches", () => {
    render(<Harness />);
    const box = open();

    fireEvent.change(box, { target: { value: "no-such-strategy" } });

    expect(screen.queryAllByRole("option")).toHaveLength(0);
    expect(screen.getByText(/no versions match/i)).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// Grouping and the option text
// ---------------------------------------------------------------------------

describe("VersionPicker (grouping)", () => {
  it("groups by strategy in catalog order, versions in version_tree order", () => {
    const { container } = render(<Harness />);
    open();

    // The group HEADERS (the popup's own `vp-group` lines), in catalog order.
    const headers = [...container.querySelectorAll(".vp-group")].map(
      (header) => header.textContent ?? "",
    );
    expect(headers).toHaveLength(12);
    expect(headers[0]).toBe("Aurora Momentum");
    expect(headers[1]).toBe(DUP_NAME + " · 2026-09-20 UTC · dupe20");
    expect(headers[11]).toBe("Lima Carry");

    // Versions inside the first group are v1…v10, in order.
    expect(optionTexts().slice(0, 10)).toEqual([
      "v1 · 2026-08-01 UTC · 1aurora · human",
      "v2 · 2026-08-02 UTC · 2aurora · human",
      "v3 · 2026-08-03 UTC · 3aurora · human",
      "v4 · 2026-08-04 UTC · 4aurora · human",
      "v5 · 2026-08-05 UTC · 5aurora · human",
      "v6 · 2026-08-06 UTC · 6aurora · human",
      "v7 · 2026-08-07 UTC · 7aurora · human",
      "v8 · 2026-08-08 UTC · 8aurora · human",
      "v9 · 2026-08-09 UTC · 9aurora · human",
      "v10 · 2026-08-10 UTC · 10aurora · human",
    ]);
  });

  it("disambiguates same-named groups with the strategy's created date and short id (#318)", () => {
    const { container } = render(<Harness />);
    open();

    const headers = [...container.querySelectorAll(".vp-group")].map(
      (header) => header.textContent ?? "",
    );
    const dupHeaders = headers.filter((header) => header.includes(DUP_NAME));
    expect(dupHeaders).toEqual([
      `${DUP_NAME} · 2026-09-20 UTC · dupe20`,
      `${DUP_NAME} · 2026-09-22 UTC · dupe22`,
    ]);
    // Exactly one group per colliding strategy, and only the collision carries
    // a disambiguator.
    expect(headers.filter((header) => header.startsWith("Echo Breakout"))).toEqual([
      "Echo Breakout",
    ]);
  });

  it("renders each option as label · date · short id · creator, and marks the pinned version", () => {
    render(<Harness />);
    const box = open();

    // An external agent's version carries the Library's provenance line
    // (`external_agent · <agent name>`); a human's is the bare label.
    fireEvent.change(box, { target: { value: "golf trend" } });
    expect(screen.getAllByRole("option")[0]?.textContent).toBe(
      "v1 · 2026-08-01 UTC · 1golf77 · external_agent · seed-r4s5",
    );

    fireEvent.change(box, { target: { value: "delta reversion" } });
    const texts = optionTexts();
    expect(texts[3]).toBe("v4 · 2026-08-04 UTC · 4delta4 · human · pinned");
    expect(texts[0]).toBe("v1 · 2026-08-01 UTC · 1delta4 · human");
  });
});

// ---------------------------------------------------------------------------
// Keyboard
// ---------------------------------------------------------------------------

describe("VersionPicker (keyboard)", () => {
  it("keeps focus in the input and opens with the selected option active", () => {
    render(<Harness />);
    const box = open();

    expect(document.activeElement).toBe(box);
    expect(box.getAttribute("aria-expanded")).toBe("true");
    expect(activeOptionText()).toBe("v1 · 2026-08-01 UTC · 1aurora · human");
  });

  it("moves the active option with ↓/↑ (clamped) and selects it with Enter", () => {
    const onSelect = vi.fn();
    render(<Harness onSelect={onSelect} />);
    const box = open();

    fireEvent.keyDown(box, { key: "ArrowDown" });
    fireEvent.keyDown(box, { key: "ArrowDown" });
    expect(activeOptionText()).toBe("v3 · 2026-08-03 UTC · 3aurora · human");

    // ↑ walks back; at the first option it stays there.
    fireEvent.keyDown(box, { key: "ArrowUp" });
    fireEvent.keyDown(box, { key: "ArrowUp" });
    fireEvent.keyDown(box, { key: "ArrowUp" });
    expect(activeOptionText()).toBe("v1 · 2026-08-01 UTC · 1aurora · human");

    // Enter selects the ACTIVE option — v2 after one ↓ — and closes the list.
    fireEvent.keyDown(box, { key: "ArrowDown" });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(onSelect).toHaveBeenCalledExactlyOnceWith("2aurora");
    expect(box.value).toBe("v2 · 2026-08-02 UTC · 2aurora · human");
    expect(screen.queryAllByRole("option")).toHaveLength(0);
    expect(document.activeElement).toBe(box);
  });

  it("clamps at the last option and jumps with Home/End", () => {
    render(<Harness />);
    const box = open();

    fireEvent.change(box, { target: { value: "juliet range" } });
    expect(screen.getAllByRole("option")).toHaveLength(10);

    fireEvent.keyDown(box, { key: "End" });
    expect(activeOptionText()).toBe("v10 · 2026-08-10 UTC · 10juliet · human");
    // Ten more ↓ keep it at the last option rather than wrapping.
    for (let i = 0; i < 10; i += 1) {
      fireEvent.keyDown(box, { key: "ArrowDown" });
    }
    expect(activeOptionText()).toBe("v10 · 2026-08-10 UTC · 10juliet · human");

    fireEvent.keyDown(box, { key: "Home" });
    expect(activeOptionText()).toBe("v1 · 2026-08-01 UTC · 1juliet · human");
  });

  it("closes on Escape without selecting, restoring the selected option's text", () => {
    const onSelect = vi.fn();
    render(<Harness onSelect={onSelect} />);
    const box = open();

    fireEvent.change(box, { target: { value: "lima carry" } });
    fireEvent.keyDown(box, { key: "Escape" });

    expect(onSelect).not.toHaveBeenCalled();
    expect(screen.queryAllByRole("option")).toHaveLength(0);
    expect(box.getAttribute("aria-expanded")).toBe("false");
    expect(box.value).toBe("v1 · 2026-08-01 UTC · 1aurora · human");
    expect(document.activeElement).toBe(box);
  });

  it("selects with the mouse without stealing focus from the input", () => {
    const onSelect = vi.fn();
    render(<Harness onSelect={onSelect} />);
    const box = open();

    fireEvent.change(box, { target: { value: "echo breakout" } });
    const option = screen.getAllByRole("option")[2] as HTMLElement;
    fireEvent.mouseDown(option);
    fireEvent.click(option);

    expect(onSelect).toHaveBeenCalledExactlyOnceWith("3echo55");
    expect(box.value).toBe("v3 · 2026-08-03 UTC · 3echo55 · human");
    expect(document.activeElement).toBe(box);
  });
});

// ---------------------------------------------------------------------------
// Disabled
// ---------------------------------------------------------------------------

describe("VersionPicker (disabled)", () => {
  it("is disabled while busy: no popup, no keyboard handling", () => {
    const onSelect = vi.fn();
    render(
      <VersionPicker
        strategies={STRATEGIES}
        value={FIRST_VERSION}
        onChange={onSelect}
        disabled
      />,
    );

    const box = screen.getByRole("combobox") as HTMLInputElement;
    expect(box.disabled).toBe(true);
    fireEvent.focus(box);
    fireEvent.click(box);
    expect(screen.queryAllByRole("option")).toHaveLength(0);
    fireEvent.keyDown(box, { key: "ArrowDown" });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(onSelect).not.toHaveBeenCalled();
    expect(box.value).toBe("v1 · 2026-08-01 UTC · 1aurora · human");
  });
});
