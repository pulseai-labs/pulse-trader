// Cross-screen requests that carry an id (r3.s4.w5).
//
// The route table is append-only and its fragments are exact (`#/deploy`,
// `#/backtest`) — an id cannot ride the hash without changing route
// resolution for every screen. A module-level request the destination
// consumes on mount (or on the next hash change when it is already mounted)
// keeps the table's shape and gives "open the Lab on THIS version" and "open
// THIS session's detail" a real mechanism instead of a fabricated default.

let requestedBacktestVersion: string | null = null;
let requestedSession: string | null = null;

/** Ask for the Backtest Lab, opened on one version. */
export function openBacktestLab(versionId: string): void {
  requestedBacktestVersion = versionId;
  window.location.hash = "#/backtest";
}

/** The pending Backtest Lab request, consumed once. */
export function takeRequestedBacktestVersion(): string | null {
  const versionId = requestedBacktestVersion;
  requestedBacktestVersion = null;
  return versionId;
}

/** Ask for the Deployment Dashboard, opened on one session's detail. */
export function openSessionDetail(sessionId: string): void {
  requestedSession = sessionId;
  window.location.hash = "#/deploy";
}

/** The pending session-detail request, consumed once. */
export function takeRequestedSession(): string | null {
  const sessionId = requestedSession;
  requestedSession = null;
  return sessionId;
}

/** The title strip's paper-count poll (App) subscribes here (r4.s5.w4, #335). */
type PaperCountListener = () => void;

const paperCountListeners = new Set<PaperCountListener>();

/**
 * Subscribe the title strip's existing count poll to promote-succeeded
 * requests (#335). Returns the unsubscribe.
 */
export function subscribePaperCountRefresh(listener: PaperCountListener): () => void {
  paperCountListeners.add(listener);
  return () => {
    paperCountListeners.delete(listener);
  };
}

/**
 * Ask the title strip's count to run its existing read NOW (#335): a promote
 * that succeeded changes the count before the 15 s tick would, and this is a
 * request to the one poll App already runs — never a second poll.
 */
export function requestPaperCountRefresh(): void {
  for (const listener of paperCountListeners) {
    listener();
  }
}
