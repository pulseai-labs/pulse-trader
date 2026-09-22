// The Strategy Library — the app's first real screen (r1.s1.w3).
//
// Renders ONLY what the `library_overview` payload carries: every persisted
// strategy, each strategy's version tree, per-version stats where a persisted
// run exists and an em dash where one does not (grill A1), and — through the
// third `.layout` track the shell opens for this route — the selected version's
// details pane. Every figure on screen arrives pre-formatted from the ring
// (`src/tauri/library.rs`), so the screen does no numeric math and cannot
// invent a number. No sample rows, no invented metrics, no mock's "pair" /
// "timeframes" lines (`StrategyDsl` has neither field).
//
// Convention (UnbuiltScreen.tsx is the worked example): one file per screen,
// default export, a component that takes NO props — the shell mounts it from
// the route table; all state is its own.
//
// The details pane reaches the third track through a PORTAL: `App.tsx` owns the
// track (it renders the `<aside id="details-pane">` host when the route
// declares `details: true` — table-driven, G7), and this screen owns what fills
// it. The host is looked up in an effect, not during render, because it is
// committed in the same tree this screen mounts in.

import { useCallback, useEffect, useMemo, useState } from "react";
import type { CSSProperties } from "react";
import { createPortal } from "react-dom";

import { commands } from "../bindings";
import type { LibraryOverview, LibraryStrategy, LibraryVersion, VersionStats } from "../bindings";
import { useRefetchOnFocus } from "../hooks/useRefetchOnFocus";

/** The em dash a version with no persisted run renders (grill A1) — a statement
 * that no run exists, never a zero dressed up as data. */
const EM_DASH = "—";

/** The details-track host `App.tsx` renders when the route declares the track. */
const DETAILS_PANE_ID = "details-pane";

// Tree geometry (px) — proportions read from the design reference, authored
// here rather than ported (`check-design-system.sh` forbids the port).
const NODE_W = 134;
const NODE_H = 64;
const NODE_PAD = 8;
const COL_W = 158;
const ROW_H = 78;

/** Which version the details pane is showing — the ids, not a payload object,
 * so a focus refetch re-renders the pane from the fresher `overview` (F5). */
interface SelectionId {
  strategyId: string;
  versionId: string;
}

/** The resolved selection the details pane renders. */
interface Selection {
  strategyName: string;
  label: string;
  version: LibraryVersion;
}

export default function LibraryScreen() {
  const [overview, setOverview] = useState<LibraryOverview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [selected, setSelected] = useState<SelectionId | null>(null);
  const [paneHost, setPaneHost] = useState<HTMLElement | null>(null);

  /**
   * Read the library overview — the mount fetch AND the refetch-on-focus read
   * (r2.s1.w4 C2), one code path.
   *
   * `alive` is the caller's: the mount effect passes its own cleanup flag so a
   * response arriving after unmount sets no state. A refetch never clears the
   * selection — a version selected from the previous payload stays selected,
   * and the details pane re-renders from the fresher object. A successful read
   * also clears a stale error — a recovered backend must not leave the
   * library hidden behind a dead read (F5).
   */
  const load = useCallback(async (alive: () => boolean = () => true): Promise<void> => {
    try {
      const result = await commands.libraryOverview();
      if (!alive()) return;
      if (result.status === "ok") {
        setOverview(result.data);
        setError(null);
      } else {
        setError(result.error.message);
      }
    } catch {
      // A rejection here is the IPC call itself failing (no app handle under
      // a non-Tauri preview) — rendered as the honest error line, never as
      // fabricated content.
      if (alive()) setError("The library read failed.");
    }
  }, []);

  useEffect(() => {
    let alive = true;
    void load(() => alive);
    return () => {
      alive = false;
    };
  }, [load]);

  // C2: versions and runs written by `pulse mcp` appear on their own when the
  // window regains focus — DOM events only, throttled inside the hook.
  useRefetchOnFocus(load);

  useEffect(() => {
    setPaneHost(document.getElementById(DETAILS_PANE_ID));
  }, []);

  // F5: the selection is resolved against the FRESHEST overview — a refetch
  // swaps `overview` and this memo re-derives the version object, so the
  // details pane never renders a stale payload. A version absent from the
  // fresh payload resolves to null (the hint), not a phantom.
  const selection = useMemo<Selection | null>(() => {
    if (selected === null || overview === null) return null;
    const strategy = overview.strategies.find((s) => s.id === selected.strategyId);
    const version = strategy?.versions.find((v) => v.id === selected.versionId);
    if (strategy === undefined || version === undefined) return null;
    return {
      strategyName: strategy.name,
      label: versionLabels(strategy.versions).get(version.id) ?? "?",
      version,
    };
  }, [selected, overview]);

  if (error !== null) {
    return (
      <div className="lib-error" role="alert">
        {error}
      </div>
    );
  }
  if (overview === null) {
    return null;
  }

  const totalVersions = overview.strategies.reduce((sum, s) => sum + s.versions.length, 0);

  return (
    <div className="library">
      <div className="ctool">
        <div className="ctool-left">
          <h1 className="ctool-title">Strategy Library</h1>
          <span className="ctool-count">
            {overview.strategies.length} {overview.strategies.length === 1 ? "strategy" : "strategies"} ·{" "}
            {totalVersions} {totalVersions === 1 ? "version" : "versions"}
          </span>
        </div>
        {/* Search, view-mode segments and the "New strategy" button are r1
            dead controls — omitted rather than rendered inert. */}
      </div>

      {overview.strategies.length === 0 ? (
        // G4: a designed first run, not a void — one line naming the single
        // next action. The Designer is one nav row away; naming it is honest
        // whether that screen has landed yet or not.
        <div className="lib-empty">
          <h2 className="lib-empty-title">No strategies yet</h2>
          <p className="lib-empty-body">
            Describe a strategy in the Strategy Designer — it will appear here.
          </p>
        </div>
      ) : (
        <div className="strategies">
          {overview.strategies.map((strategy) => (
            <StrategyCard
              key={strategy.id}
              strategy={strategy}
              expanded={expanded[strategy.id] === true}
              onToggle={() =>
                setExpanded((current) => ({ ...current, [strategy.id]: !(current[strategy.id] === true) }))
              }
              onSelect={(version) =>
                setSelected({ strategyId: strategy.id, versionId: version.id })
              }
              selectedId={selected?.versionId ?? null}
            />
          ))}
        </div>
      )}

      {paneHost !== null && createPortal(<DetailsPane selection={selection} />, paneHost)}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Strategy card
// ---------------------------------------------------------------------------

function StrategyCard({
  strategy,
  expanded,
  onToggle,
  onSelect,
  selectedId,
}: {
  strategy: LibraryStrategy;
  expanded: boolean;
  onToggle: () => void;
  onSelect: (version: LibraryVersion) => void;
  selectedId: string | null;
}) {
  const labels = versionLabels(strategy.versions);
  const latest = latestVersion(strategy);

  return (
    <div className={`scard${expanded ? " is-expanded" : ""}`}>
      <header className="scard-head">
        <button
          type="button"
          className="chev"
          aria-label={`Toggle ${strategy.name}`}
          onClick={onToggle}
        >
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path
              d="M3 1 L7 5 L3 9"
              stroke="currentColor"
              strokeWidth="1.4"
              fill="none"
              strokeLinecap="round"
              strokeLinejoin="round"
              style={{
                transition: "transform 180ms var(--ease-out)",
                transformOrigin: "5px 5px",
                transform: expanded ? "rotate(90deg)" : "none",
              }}
            />
          </svg>
        </button>

        <div className="scard-id">
          {strategy.pinnedVersionId !== null && (
            <span className="scard-pin" title="Pinned version">
              ★
            </span>
          )}
          <span className="scard-name">{strategy.name}</span>
          <span className="scard-meta">
            {strategy.versions.length} {strategy.versions.length === 1 ? "version" : "versions"} ·
            created {datePart(strategy.createdAt)}
          </span>
        </div>
        {/* The mock's state dot, sparkline and card actions are omitted: the
            record carries no state/pair/tf, and every action needs a write
            command this item does not have. */}
      </header>

      {expanded ? (
        <div className="scard-body">
          <VersionTree
            strategy={strategy}
            labels={labels}
            onSelect={onSelect}
            selectedId={selectedId}
          />
        </div>
      ) : (
        latest !== undefined && (
          <div className="scard-collapsed">
            <span className="cc-label">latest</span>
            <span className="cc-name">{labels.get(latest.id)}</span>
            <span className="cc-summary">{summaryLine(latest)}</span>
            <span className="cc-kpis mono">
              <Kpis stats={latest.stats} />
            </span>
          </div>
        )
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Version tree
// ---------------------------------------------------------------------------

/** A node's grid position, in tree-local pixels. */
interface Placed {
  x: number;
  y: number;
}

/**
 * Position every version: depth places the column, a post-order walk places the
 * rows (leaves in payload order, a parent at its first child's row) — the tidy
 * shape of the design reference. Roots (and versions whose parent is not in the
 * payload) start their own column-0 row, so nothing is ever dropped.
 */
function layoutTree(versions: readonly LibraryVersion[]): Map<string, Placed> {
  const ids = new Set(versions.map((v) => v.id));
  const children = new Map<string, string[]>();
  const roots: string[] = [];
  for (const v of versions) {
    if (v.parentId !== null && ids.has(v.parentId)) {
      const list = children.get(v.parentId) ?? [];
      list.push(v.id);
      children.set(v.parentId, list);
    } else {
      roots.push(v.id);
    }
  }

  const depthOf = new Map<string, number>();
  const placed = new Map<string, Placed>();
  let rowCursor = 0;

  const place = (id: string, depth: number): number => {
    depthOf.set(id, depth);
    const kids = children.get(id) ?? [];
    if (kids.length === 0) {
      const y = rowCursor * ROW_H;
      placed.set(id, { x: depth * COL_W, y });
      rowCursor += 1;
      return y;
    }
    let top = Number.POSITIVE_INFINITY;
    for (const kid of kids) {
      top = Math.min(top, place(kid, depth + 1));
    }
    placed.set(id, { x: depth * COL_W, y: top });
    return top;
  };

  for (const root of roots) {
    place(root, 0);
  }
  return placed;
}

function VersionTree({
  strategy,
  labels,
  onSelect,
  selectedId,
}: {
  strategy: LibraryStrategy;
  labels: Map<string, string>;
  onSelect: (version: LibraryVersion) => void;
  selectedId: string | null;
}) {
  const placed = useMemo(() => layoutTree(strategy.versions), [strategy]);

  const { edges, width, height } = useMemo(() => {
    const edgeList: { key: string; d: string }[] = [];
    let maxDepth = 0;
    let maxRow = 0;
    for (const v of strategy.versions) {
      const node = placed.get(v.id);
      if (node === undefined) continue;
      maxDepth = Math.max(maxDepth, node.x / COL_W);
      maxRow = Math.max(maxRow, node.y / ROW_H);
      if (v.parentId === null) continue;
      const parent = placed.get(v.parentId);
      if (parent === undefined) continue;
      const x1 = parent.x + NODE_W + NODE_PAD;
      const y1 = parent.y + NODE_H / 2 + NODE_PAD;
      const x2 = node.x + NODE_PAD;
      const y2 = node.y + NODE_H / 2 + NODE_PAD;
      const cx1 = x1 + (x2 - x1) * 0.5;
      const cx2 = x2 - (x2 - x1) * 0.5;
      edgeList.push({
        key: `${v.parentId}->${v.id}`,
        d: `M${x1},${y1} C${cx1},${y1} ${cx2},${y2} ${x2},${y2}`,
      });
    }
    return {
      edges: edgeList,
      width: (maxDepth + 1) * COL_W + NODE_W + NODE_PAD * 2,
      height: (maxRow + 1) * ROW_H + NODE_H + NODE_PAD * 2,
    };
  }, [placed, strategy.versions]);

  return (
    <div className="vtree-wrap" style={{ height }}>
      <svg className="vtree-svg" width={width} height={height} aria-hidden="true">
        {edges.map((edge) => (
          <path
            key={edge.key}
            d={edge.d}
            stroke="var(--line-3)"
            strokeWidth="1.25"
            fill="none"
            strokeLinecap="round"
          />
        ))}
      </svg>
      {strategy.versions.map((v) => {
        const node = placed.get(v.id);
        if (node === undefined) return null;
        return (
          <VersionNode
            key={v.id}
            version={v}
            label={labels.get(v.id) ?? "?"}
            pinned={strategy.pinnedVersionId === v.id}
            selected={selectedId === v.id}
            style={{
              position: "absolute",
              left: node.x + NODE_PAD,
              top: node.y + NODE_PAD,
              width: NODE_W,
              height: NODE_H,
            }}
            onClick={() => onSelect(v)}
          />
        );
      })}
    </div>
  );
}

function VersionNode({
  version,
  label,
  pinned,
  selected,
  style,
  onClick,
}: {
  version: LibraryVersion;
  label: string;
  pinned: boolean;
  selected: boolean;
  style: CSSProperties;
  onClick: () => void;
}) {
  const delta = version.deltaVsParent;
  const down = delta !== null && delta.startsWith("-");
  return (
    <button
      type="button"
      className={`vnode${selected ? " is-selected" : ""}`}
      style={style}
      onClick={onClick}
      aria-label={`Version ${label}`}
      aria-pressed={selected}
    >
      <div className="vnode-head">
        <span className="vnode-name">{label}</span>
        {pinned && (
          <span className="vnode-pin" title="Pinned version">
            ★
          </span>
        )}
        {/* d22 (r2.s3.w4): the certification badge follows the version's
            LATEST walk-forward run — rendered when `certified` is true, absent
            entirely when false so the mark can't fade into decoration. */}
        {version.certified && (
          <span className="vnode-cert" title="Latest walk-forward passed">
            certified
          </span>
        )}
        {delta !== null && (
          <span className={`vnode-delta mono ${down ? "bear" : "bull"}`}>
            {down ? "▼" : "▲"} {delta}
          </span>
        )}
      </div>
      {/* C1 (r2.s1.w4): who authored the version — `external_agent · <agent
          name>` for an agent-written version, the bare label otherwise. The
          hypothesis rides a clamped subline only when the version carries one. */}
      <div
        className={`vnode-provenance${version.createdBy === "external_agent" ? " agent" : ""}`}
      >
        {provenanceLine(version)}
      </div>
      {version.hypothesis !== null && (
        <div className="vnode-hypothesis">{version.hypothesis}</div>
      )}
      <div className="vnode-summary">{summaryLine(version)}</div>
      <div className="vnode-kpis mono">
        <Kpis stats={version.stats} />
      </div>
    </button>
  );
}

// ---------------------------------------------------------------------------
// Details pane (portaled into the shell's third track)
// ---------------------------------------------------------------------------

function DetailsPane({ selection }: { selection: Selection | null }) {
  if (selection === null) {
    // The route still declares the track — the Library is two-pane by design;
    // with nothing selected, the pane holds this hint, not fabricated content.
    return <div className="pane-hint">Select a version to see its details.</div>;
  }

  const { strategyName, label, version } = selection;
  const stats = version.stats;

  return (
    <>
      <header className="details-head">
        <div className="details-breadcrumb">
          <span className="db-strat">{strategyName}</span>
          <span className="db-sep">/</span>
          <span className="db-ver mono">{label}</span>
        </div>
      </header>

      <div className="details-kpis">
        <div className="dkpi">
          <span className="dkpi-lab">expectancy</span>
          <span className={`dkpi-val mono${toneClass(stats?.expectancy)}`}>
            {stats?.expectancy ?? EM_DASH}
          </span>
        </div>
        <div className="dkpi">
          <span className="dkpi-lab">win rate</span>
          <span className="dkpi-val mono">{stats?.winRate ?? EM_DASH}</span>
        </div>
        <div className="dkpi">
          <span className="dkpi-lab">trades</span>
          <span className="dkpi-val mono">{stats?.trades ?? EM_DASH}</span>
        </div>
      </div>

      <section className="dsl-section">
        <h4 className="dsl-h">DSL summary</h4>
        <DslBlock title="Setup" lines={[`name: ${version.dsl.name}`, `direction: ${version.dsl.direction}`]} />
        <DslBlock title="Entries" lines={version.dsl.entry} />
        <DslBlock title="Filters" lines={version.dsl.filters} empty="none" />
        <DslBlock title="Exits" lines={version.dsl.exits} />
        <DslBlock title="Risk" lines={version.dsl.risk} />
        {/* The mock's `pair` / `timeframes` lines are deliberately absent:
            `StrategyDsl` carries neither field. */}
      </section>

      {/* d22 (r2.s3.w4): certification state is part of the version's record,
          so the pane always states it — "certified" or "uncertified" — and
          names the certifying walk-forward run only when the pointer is set. */}
      <section className="d-section">
        <h4 className="dsl-h">Certification</h4>
        <div className="d-cert">
          <span className={`d-cert-state${version.certified ? " is-certified" : ""}`}>
            {version.certified ? "certified" : "uncertified"}
          </span>
          {version.latestWalkForwardRunId !== null && (
            <span className="d-cert-run mono">{version.latestWalkForwardRunId}</span>
          )}
        </div>
      </section>

      {/* C1 (r2.s1.w4): the agent's stated hypothesis in full — the node's
          subline is CSS-clamped to two lines, this is the unclamped text. A
          human version has none, so the block is absent rather than empty. */}
      {version.hypothesis !== null && (
        <section className="d-section">
          <h4 className="dsl-h">Hypothesis</h4>
          <p className="d-hypothesis">{version.hypothesis}</p>
        </section>
      )}

      <section className="d-section">
        <h4 className="dsl-h">Recent backtests</h4>
        {version.recentRuns.length === 0 ? (
          <div className="bt-none dim">None yet.</div>
        ) : (
          <div className="bt-list">
            {version.recentRuns.map((run) => (
              <div key={run.id} className="bt-row">
                <span className="bt-id mono">{run.id.slice(0, 8)}</span>
                <span className="bt-range">{datePart(run.createdAt)}</span>
                <span className="bt-spacer" />
                <span className={`mono ${run.expectancy.startsWith("-") ? "bear" : "bull"}`}>
                  {run.expectancy}
                </span>
                <span className="mono dim">{run.trades}t</span>
              </div>
            ))}
          </div>
        )}
      </section>

      {/* "Recent coaching" is not rendered AT ALL (grill A3) — the coach is
          r1.s4, and a rendered-empty block would overstate it. Action buttons
          are omitted the same way: each needs a write command. */}
    </>
  );
}

function DslBlock({ title, lines, empty }: { title: string; lines: string[]; empty?: string }) {
  return (
    <div className="dsl-block">
      <div className="dsl-block-head">
        <span className="dsl-block-title">{title}</span>
        <span className="dsl-block-count mono">{lines.length}</span>
      </div>
      <div className="dsl-block-body">
        {lines.length === 0 && empty !== undefined ? (
          <div className="dsl-text mono dim">{empty}</div>
        ) : (
          lines.map((line, index) => (
            <div key={index} className="dsl-text mono">
              {line}
            </div>
          ))
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Small shared helpers (pure — payload in, display out)
// ---------------------------------------------------------------------------

/** The three headline KPIs, or a single em dash when no run exists (A1). */
function Kpis({ stats }: { stats: VersionStats | null }) {
  if (stats === null) {
    return <>{EM_DASH}</>;
  }
  return (
    <>
      <span className={stats.expectancy.startsWith("-") ? "bear" : "bull"}>{stats.expectancy}</span>
      <span className="kpi-sep">·</span>
      <span>{stats.winRate}</span>
      <span className="kpi-sep">·</span>
      <span>{stats.trades}t</span>
    </>
  );
}

/** `v1`-style ordinals from the payload's own parent-ordered list. */
function versionLabels(versions: readonly LibraryVersion[]): Map<string, string> {
  return new Map(versions.map((v, index) => [v.id, `v${index + 1}`]));
}

/** The most recently created version (`version_tree` order is topological, not
 * chronological — RFC3339 UTC strings compare correctly as text). */
function latestVersion(strategy: LibraryStrategy): LibraryVersion | undefined {
  return strategy.versions.reduce<LibraryVersion | undefined>(
    (latest, v) => (latest === undefined || v.createdAt > latest.createdAt ? v : latest),
    undefined,
  );
}

/** A version's provenance label (r2.s1.w4 C1): `external_agent · <agent name>`
 * for an agent-written version, the bare `created_by` label for everything
 * else. An agent version whose submission row is absent still reads
 * `external_agent` — the label, never a guessed name. */
function provenanceLine(version: LibraryVersion): string {
  return version.agentName !== null
    ? `${version.createdBy} · ${version.agentName}`
    : version.createdBy;
}

/** A version's one-line summary from its own DSL fields. */
function summaryLine(version: LibraryVersion): string {
  const entry = version.dsl.entry[0] ?? "(no entry)";
  return `${version.dsl.direction} · ${entry}`;
}

/** The `YYYY-MM-DD` slice of an RFC3339 timestamp — the record's own date. */
function datePart(timestamp: string): string {
  return timestamp.slice(0, 10);
}

/** The bull/bear class for a signed display string, if one applies. */
function toneClass(value: string | undefined): string {
  if (value === undefined || value.startsWith("-")) return value === undefined ? "" : " bear";
  return " bull";
}
