// The streak and drawdown card (r5.s4.w2) — what a NORMAL bad run of this
// strategy looks like, shown before the trader promotes it to paper.
//
// The card is a pure function of the DTO `streak_card` answers (w1's read
// contract): the wire is tagged on `state` as `ready` | `notEnoughTrades` |
// `noRun`, and this component adds the two states the SCREEN owns — `loading`
// (the read is in flight) and `failed` (it rejected, or the bus refused it).
// None of the five invents a figure (PDR-0011): a state without figures says so
// in words, and never shows zeros as if they were results.
//
// Every Decimal string (the two drawdowns and the percentage) crosses UNPADDED
// — `"10"`, not `"10.00"` — so each renders through the format module's decimal
// text helpers and `Figure` keeps the server's exact text one hover away. The
// server fixes the arithmetic (rounded half-even to 2 dp); this card fixes the
// padding, and never the other way round. The counts (trades, simulated runs)
// are DTO numbers and render as counts.
//
// Units (SPINE.md grill Q3): the longest losing streak in trades; the max
// drawdown in R and in USDT, the USDT figure as a percentage of the fixed
// 10,000 USDT equity. The run named is the one the DTO names (Q4) — the
// version's latest full-span run, never the sheet's shape run.

import type { ReactNode } from "react";

import type { StreakCardDto } from "../bindings";
import { Figure } from "../format/Figure";
import { decimal, usdt } from "../format/numbers";
import { shortId } from "./SessionBadge";

/** What the Promote sheet holds for the card: the wire's three states plus the
 * two the screen owns. */
export type StreakCardState = { state: "loading" } | { state: "failed" } | StreakCardDto;

/** A count as the reader counts it — the source line's `10,000`. */
function count(value: number): string {
  return value.toLocaleString("en-US");
}

/** The card's title row: the block's own name, and whatever it has to say
 * beside it. */
function TitleRow({ children }: { children: ReactNode }) {
  return (
    <div className="ps-row">
      <span className="ps-lab">A normal bad run</span>
      <span className="sc-source">{children}</span>
    </div>
  );
}

/** One drawdown cell: the R magnitude and, on the same row, the USDT figure
 * with its share of the fixed 10,000 USDT equity — one path's own pair. */
function DrawdownCell({ r, usdtText, pct }: { r: string; usdtText: string; pct: string }) {
  return (
    <>
      <Figure exact={r}>{decimal(r, 2)} R</Figure>
      <span className="dim"> · </span>
      <Figure exact={usdtText}>{usdt(usdtText)}</Figure>
      <span className="dim">
        {" ("}
        <Figure exact={pct}>{decimal(pct, 2)}</Figure>
        {"% of 10,000)"}
      </span>
    </>
  );
}

export function StreakCard({ card }: { card: StreakCardState }) {
  if (card.state === "loading") {
    // The sheet's own loading idiom.
    return <div className="sh-box tone-neutral">Reading the streak and drawdown card…</div>;
  }
  if (card.state === "failed") {
    return <TitleRow>The streak and drawdown card could not be read.</TitleRow>;
  }
  if (card.state === "noRun") {
    return <TitleRow>No full backtest of this version yet — run one to see a normal bad run.</TitleRow>;
  }
  if (card.state === "notEnoughTrades") {
    return (
      <TitleRow>
        {`Only ${count(card.closedTrades)} closed trades in backtest ${shortId(card.runId)} — at least 30 are needed to estimate a normal bad run.`}
      </TitleRow>
    );
  }
  return (
    <div className="sc-card">
      <TitleRow>
        {"From backtest "}
        <span className="mono" title={card.runId}>
          {shortId(card.runId)}
        </span>
        {` · ${count(card.closedTrades)} trades · ${count(card.paths)} simulated runs of ${count(card.horizonTrades)} trades`}
      </TitleRow>
      <table className="sc-table" aria-label="A normal bad run">
        <thead>
          <tr>
            <td />
            <th scope="col">Typical (p50)</th>
            <th scope="col">Bad but normal (p95)</th>
            <th scope="col">This backtest</th>
          </tr>
        </thead>
        <tbody>
          <tr>
            <th scope="row">Longest losing streak</th>
            <td className="mono">{`${count(card.streak.p50)} trades`}</td>
            <td className="mono">{`${count(card.streak.p95)} trades`}</td>
            <td className="mono">{`${count(card.streak.observed)} trades`}</td>
          </tr>
          <tr>
            <th scope="row">Max drawdown</th>
            <td className="mono">
              <DrawdownCell
                r={card.drawdownR.p50}
                usdtText={card.drawdownUsdt.p50}
                pct={card.drawdownPct.p50}
              />
            </td>
            <td className="mono">
              <DrawdownCell
                r={card.drawdownR.p95}
                usdtText={card.drawdownUsdt.p95}
                pct={card.drawdownPct.p95}
              />
            </td>
            <td className="mono">
              <DrawdownCell
                r={card.drawdownR.observed}
                usdtText={card.drawdownUsdt.observed}
                pct={card.drawdownPct.observed}
              />
            </td>
          </tr>
        </tbody>
      </table>
      <p className="sh-note">
        {"Expect runs like these before the strategy is wrong. They come from this "}
        {"backtest's own trades, drawn at random with a fixed seed."}
      </p>
    </div>
  );
}
