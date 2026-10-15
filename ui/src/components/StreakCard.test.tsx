// Rendered tests for the streak and drawdown card (r5.s4.w2) — the block the
// Promote sheet shows before the confirm button.
//
// The card is a pure function of the DTO `streak_card` answers (w1's contract):
// every state the wire can carry gets its own plain rendering, none of them
// invents a figure (PDR-0011), and every Decimal string crosses UNPADDED
// (`"10"`), so the display text here is the padding rule's own proof.

import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { StreakCardDto } from "../bindings";
import { StreakCard } from "./StreakCard";

// ---------------------------------------------------------------------------
// Fixtures (mirroring the generated types exactly)
// ---------------------------------------------------------------------------

/** The card the engine answers for a version whose latest full-span run has
 * 179 closed trades. The decimal strings are the server's own text: the
 * arithmetic is fixed server-side, the padding is not. */
function ready(): StreakCardDto {
  return {
    state: "ready",
    runId: "run-full-span-1",
    runCreatedAt: "2026-10-01T09:00:00.000Z",
    closedTrades: 179,
    horizonTrades: 100,
    paths: 10000,
    seed: "0x523553345354524b",
    streak: { p50: 7, p95: 12, observed: 3 },
    drawdownR: { p50: "10", p95: "19", observed: "3" },
    drawdownUsdt: { p50: "1000", p95: "1900", observed: "300" },
    drawdownPct: { p50: "10", p95: "19", observed: "3" },
  };
}

/** One data row's rendered cells, found by its row header. */
function cells(label: string): (string | null)[] {
  const header = screen.getByRole("rowheader", { name: label });
  const row = header.closest("tr");
  if (row === null) throw new Error(`no row rendered for ${label}`);
  return within(row)
    .getAllByRole("cell")
    .map((cell) => cell.textContent);
}

// ---------------------------------------------------------------------------

describe("the ready card", () => {
  it("renders the source line, the three columns and the run's own figures", () => {
    render(<StreakCard card={ready()} />);

    expect(screen.getByText("A normal bad run")).toBeTruthy();
    expect(screen.getByText("Typical (p50)")).toBeTruthy();
    expect(screen.getByText("Bad but normal (p95)")).toBeTruthy();
    expect(screen.getByText("This backtest")).toBeTruthy();

    expect(screen.getByText(/^From backtest/).textContent).toBe(
      "From backtest run-full · 179 trades · 10,000 simulated runs of 100 trades",
    );

    expect(cells("Longest losing streak")).toEqual(["7 trades", "12 trades", "3 trades"]);
    // `"10"` crosses unpadded and renders `10.00%` — the server fixed the
    // arithmetic, the card fixes the padding, and never the other way round.
    expect(cells("Max drawdown")).toEqual([
      "10.00 R · 1,000.00 USDT (10.00% of 10,000)",
      "19.00 R · 1,900.00 USDT (19.00% of 10,000)",
      "3.00 R · 300.00 USDT (3.00% of 10,000)",
    ]);

    expect(
      screen.getByText(
        "Expect runs like these before the strategy is wrong. They come from this backtest's own trades, drawn at random with a fixed seed.",
      ),
    ).toBeTruthy();
  });

  it("carries the server's exact text beside every figure, and the run's full id", () => {
    render(<StreakCard card={ready()} />);

    expect(screen.getByText("10.00 R").getAttribute("data-exact")).toBe("10");
    expect(screen.getByText("1,000.00 USDT").getAttribute("data-exact")).toBe("1000");
    expect(screen.getByText("10.00").getAttribute("data-exact")).toBe("10");
    // The row reads the 8-character convention; the full id is one hover away,
    // so the run the card read is never ambiguous (Q4).
    expect(screen.getByText("run-full").getAttribute("title")).toBe("run-full-span-1");
  });
});

describe("the plain states", () => {
  it("notEnoughTrades names the run and its count, and shows no figures", () => {
    render(<StreakCard card={{ state: "notEnoughTrades", runId: "run-thin-1", closedTrades: 12 }} />);

    expect(
      screen.getByText(
        "Only 12 closed trades in backtest run-thin — at least 30 are needed to estimate a normal bad run.",
      ),
    ).toBeTruthy();
    expect(screen.queryByRole("table")).toBeNull();
    expect(screen.queryByText(/trades$/)).toBeNull();
  });

  it("noRun says so plainly, with no figures", () => {
    render(<StreakCard card={{ state: "noRun" }} />);

    expect(
      screen.getByText("No full backtest of this version yet — run one to see a normal bad run."),
    ).toBeTruthy();
    expect(screen.queryByRole("table")).toBeNull();
  });

  it("failed says the card could not be read, with no figures", () => {
    render(<StreakCard card={{ state: "failed" }} />);

    expect(screen.getByText("The streak and drawdown card could not be read.")).toBeTruthy();
    expect(screen.queryByRole("table")).toBeNull();
  });

  it("loading uses the sheet's own loading idiom", () => {
    render(<StreakCard card={{ state: "loading" }} />);

    expect(screen.getByText("Reading the streak and drawdown card…")).toBeTruthy();
    expect(screen.queryByRole("table")).toBeNull();
  });
});
