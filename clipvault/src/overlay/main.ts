/** On-screen "Clip saved" banner (see `src-tauri/src/overlay.rs`). The Rust side
 * sets `window.__initialBanner` before load and calls `window.__showBanner`
 * for later banners; it also shows and hides the window. */
import "./overlay.css";

export interface Banner {
  kind: "saved" | "error";
  title: string;
  detail: string;
  seq: number;
}

declare global {
  interface Window {
    __initialBanner?: Banner | null;
    __showBanner?: (b: Banner) => void;
  }
}

const card = document.getElementById("banner")!;
const title = document.getElementById("banner-title")!;
const detail = document.getElementById("banner-detail")!;

function showBanner(b: Banner) {
  title.textContent = b.title;
  detail.textContent = b.detail;
  card.dataset.kind = b.kind;
  // Restart the entry animation for every banner, even on a reused window.
  card.classList.remove("in");
  void card.offsetWidth;
  card.classList.add("in");
}

window.__showBanner = showBanner;
if (window.__initialBanner) showBanner(window.__initialBanner);
// Dev preview in a browser: /overlay.html?demo
else if (new URLSearchParams(location.search).has("demo"))
  showBanner({ kind: "saved", title: "Clip saved", detail: "58 s · Rocket League", seq: 0 });
