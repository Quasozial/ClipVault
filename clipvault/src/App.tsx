import { api, isMock } from "./api";
import { canDecodeHevc } from "./lib/player";
import { useEffect } from "react";
import { HealthStatus, Toasts } from "./components/ui";
import { UpdateDialog } from "./components/UpdateDialog";
import { startUpdateChecks } from "./lib/updater";
import { ClipView } from "./pages/ClipView";
import { Library } from "./pages/Library";
import { Settings } from "./pages/Settings";
import { useHealth, useRoute } from "./lib/stores";
import "./styles.css";

function Logo() {
  // A vault door reduced to a ring and a bolt line: the app keeps clips safe.
  return (
    <svg width="18" height="18" viewBox="0 0 20 20" aria-hidden>
      <circle cx="10" cy="10" r="7.5" fill="none" stroke="currentColor" strokeWidth="2" />
      <path d="M10 2.5v4M10 13.5v4" stroke="currentColor" strokeWidth="2" />
      <circle cx="10" cy="10" r="2.2" fill="var(--accent)" />
    </svg>
  );
}

export default function App() {
  const route = useRoute();
  const health = useHealth();
  const section = route.page === "clip" ? "library" : route.page;
  useEffect(startUpdateChecks, []);
  // Auto keeps recording off HEVC when this player could not show it.
  useEffect(() => {
    const ms = typeof MediaSource === "undefined" ? undefined : MediaSource;
    const probe = document.createElement("video");
    const hevc = canDecodeHevc(ms && ((t) => ms.isTypeSupported(t)), (t) => probe.canPlayType(t));
    api.reportPlayback(hevc).catch(() => {});
  }, []);

  return (
    <div className="app">
      <header className="topbar">
        <a className="brand" href="#/">
          <Logo />
          ClipVault
        </a>
        <nav className="tabs" aria-label="Main">
          <a href="#/" aria-current={section === "library" ? "page" : undefined}>
            Library
          </a>
          <a href="#/settings" aria-current={section === "settings" ? "page" : undefined}>
            Settings
          </a>
        </nav>
        {isMock && <span className="mock-flag">Mock data</span>}
        <HealthStatus health={health} />
      </header>
      <main className="page">
        {route.page === "library" && <Library />}
        {route.page === "clip" && <ClipView key={route.id} id={route.id} />}
        {route.page === "settings" && <Settings section={route.section} />}
      </main>
      <Toasts />
      <UpdateDialog />
    </div>
  );
}
