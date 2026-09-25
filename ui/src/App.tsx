// The app's root composition. r1.s1.w5 built the chrome + sidebar with the
// no-credential banner as the pane's sole child; r1.s1.w6 makes the shell
// navigable (G5-G9): `location.hash` drives which nav row is active and what
// the content pane renders, through the table-driven router in `routes.ts`.
//
// No product screen mounts here -- the Library is `w3`, the Designer is `w4`.
// Every nav row lands on either a real route's `element` (none exist yet) or
// the honest `UnbuiltScreen` -- never a screen-shaped placeholder, which is
// exactly the fake `r1.s1` SPINE.md's ledger exists to catch.

import { useEffect, useState } from "react";
import type { ReactNode } from "react";

import { CredentialBanner } from "./components/CredentialBanner";
import { ActiveOperationsProvider } from "./hooks/useActiveOperations";
import { useServerStatus } from "./hooks/useServerStatus";
import ConnectScreen from "./screens/ConnectScreen";
import { NAV_ALL, Sidebar, WindowChrome } from "./shell/AppShell";
import { resolveNavId, resolveRoute } from "./routes";
import type { Route } from "./routes";
import UnbuiltScreen from "./screens/UnbuiltScreen";

const KNOWN_NAV_IDS = NAV_ALL.map((item) => item.id);

/**
 * Given a resolved route (or none), render its screen or the unbuilt pane.
 * Deliberately independent of `location.hash` / nav-id resolution, so it is
 * testable with a synthetic `Route` regardless of what `ROUTES` currently
 * contains (r1.s1.w6, spec step 7's rendered layer).
 */
export function RouteContent({ route }: { route: Route | undefined }): ReactNode {
  if (route?.element !== undefined) {
    const Element = route.element;
    return <Element />;
  }
  return <UnbuiltScreen />;
}

export function App() {
  const [navId, setNavId] = useState<string>(() =>
    resolveNavId(window.location.hash, KNOWN_NAV_IDS),
  );

  // The connection gate (r3.s3.w5): the polled status decides whether the app
  // renders at all. Until the FIRST read answers, render nothing — a flash of
  // the full app before a "not connected" gate would be a lie on the way to
  // the truth.
  const { status: serverStatus, refresh: refreshStatus } = useServerStatus();

  useEffect(() => {
    const onHashChange = () => setNavId(resolveNavId(window.location.hash, KNOWN_NAV_IDS));
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  const route = resolveRoute("/" + navId);
  const navEntry = NAV_ALL.find((item) => item.id === navId);
  const title = route?.title ?? navEntry?.label;

  // Table-driven (r1.s1.w3, G7): whether the third (360px) `.layout` track
  // exists is declared by the ROUTE (`details: true`), never by a screen-name
  // list here. The existing `.layout-no-details` modifier does the rest; the
  // shell owns the track, the screen owns what fills it — a route with
  // `details` gets an empty host `<aside>` and its screen portals the pane
  // content in (see `LibraryScreen.tsx`).
  const showDetailsPane = route?.details === true;

  if (serverStatus === null) {
    return null;
  }
  if (serverStatus.state !== "up") {
    return (
      <WindowChrome docTitle="Connect">
        <ConnectScreen status={serverStatus} onConnected={refreshStatus} />
      </WindowChrome>
    );
  }

  // r1.s4.w3 (#141): active operations are held ABOVE `RouteContent`, which is
  // the line a navigation re-mounts across. A backtest or a coach turn started in
  // the Lab therefore survives a trip to the Library and is still there — running
  // or settled — when the trader comes back, and the screen re-invokes nothing to
  // find that out. Mounting this inside a screen would put it back under the
  // remount it exists to survive.
  return (
    <ActiveOperationsProvider>
      <WindowChrome docTitle={title} serverStatus={serverStatus}>
        <div className={`layout${showDetailsPane ? "" : " layout-no-details"}`}>
          <Sidebar active={navId} />
          <main className="content">
            <CredentialBanner />
            <RouteContent route={route} />
          </main>
          {showDetailsPane && <aside className="details" id="details-pane" />}
        </div>
      </WindowChrome>
    </ActiveOperationsProvider>
  );
}
