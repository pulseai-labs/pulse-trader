// The Chart tab's canvas pane aligned to canvas v9 (r7.s2.w2; `chart.jsx` `DrawRail`,
// `Readout` and `.cv-wrap`). Over the canvas sit only the legend (top left), the holdout tag (bottom right), the
// state cards (centre) and the refusals (bottom centre), each clear of the bottom-left corner where the library
// draws its attribution logo (grill Q6). Every value is the read's own: nothing is computed here.

import { useEffect, useState } from "react";

import type { ChartDataDto } from "../../bindings";
import type { ChartAdapter } from "../../chart/adapter";
import { candleDirection } from "../../chart/layers";
import { NotBuiltYet } from "../../components/NotBuiltYet";
import { Figure } from "../../format/Figure";
import { price } from "../../format/numbers";
import { timeUtc } from "../../format/time";
import type { ChartState } from "./state";
import type { ChartView } from "./view";
import { legendBar } from "./view";

/** Canvas v9's drawing rail. The drawing tools are r7.s7's (PDR-0060 d3): shown, never hidden, never inert. */
export function DrawRail() {
  return (
    <div className="draw-rail">
      <NotBuiltYet what="Drawing tools" />
    </div>
  );
}

/** Canvas v9's `.cv-wrap`: the library's canvas, with the legend, the holdout tag, the state cards and the
 *  refusals over it. A refusal here never moves the chart (#505). */
export function ChartPane({ s, view }: { s: ChartState; view: ChartView }) {
  const { containerRef, adapterRef, phase, notice, goToNote, markRefusal } = s;
  const { dto, holdoutTag } = view;
  return (
    <div className="cv-wrap">
      <div className="cw-canvas" ref={containerRef} />
      {dto !== null && dto.candles.length > 0 && <OhlcLegend adapterRef={adapterRef} dto={dto} />}
      {holdoutTag !== null && (
        <p className="cv-ho-tag mono" role="note">
          {holdoutTag}
        </p>
      )}
      <div className="cv-over">
        {phase.kind === "loading" && (
          <p className="cw-msg" role="status">
            {phase.what}
          </p>
        )}
        {phase.kind === "failed" && (
          <p className="cw-msg" role="alert">
            {phase.message} ({phase.code})
          </p>
        )}
        {phase.kind === "ready" && dto !== null && dto.candles.length === 0 && (
          <p className="cw-msg" role="status">
            {`No ${dto.timeframe} bars for ${dto.pair} in this window.`}
          </p>
        )}

        {notice !== null && (
          <p className="cw-msg" role="status">
            {`The whole run at ${notice.timeframe} is ${notice.bars.toLocaleString("en-US")} bars — more than one read (the cap is 20,000). Pick a trade to load ${notice.timeframe} around it, or go to a date.`}
          </p>
        )}
      </div>
      <div className="cv-alerts">
        {markRefusal !== null && (
          <p className="cw-msg" role="alert">
            {markRefusal}
          </p>
        )}
        {goToNote !== null && (
          <p className="cw-msg" role="alert">
            {goToNote}
          </p>
        )}
      </div>
    </div>
  );
}

/** Canvas v9's `Readout`: the bar under the crosshair, else the last loaded bar. It owns the hover, so a moving
 *  crosshair re-renders the legend only. It mounts once a read has drawn bars, after the adapter exists. */
export function OhlcLegend({
  adapterRef,
  dto,
}: {
  adapterRef: { readonly current: ChartAdapter | null };
  dto: ChartDataDto;
}) {
  const [hoverMs, setHoverMs] = useState<string | null>(null);
  useEffect(() => {
    adapterRef.current?.onCrosshairMove(setHoverMs);
  }, [adapterRef]);
  const bar = legendBar(dto.candles, hoverMs);
  if (bar === null) return null;
  const side = candleDirection(bar);
  return (
    <div className="readout" role="group" aria-label="OHLC legend">
      <div className="ro-l1">
        <b className="mono">{`${dto.pair} · ${dto.timeframe}`}</b>
        <span className="dim mono">{timeUtc(bar.t)}</span>
        <span className="mono">O <b className={side}><Figure exact={bar.o}>{price(bar.o)}</Figure></b></span>
        <span className="mono">H <b className={side}><Figure exact={bar.h}>{price(bar.h)}</Figure></b></span>
        <span className="mono">L <b className={side}><Figure exact={bar.l}>{price(bar.l)}</Figure></b></span>
        <span className="mono">C <b className={side}><Figure exact={bar.c}>{price(bar.c)}</Figure></b></span>
        <NotBuiltYet what="Change from the previous close" />
      </div>
      <div className="ro-row">
        <span className="ro-sw" />
        <span className="ro-k">Anatomy</span>
        <NotBuiltYet what="Candle anatomy" />
      </div>
    </div>
  );
}
