import { relaunch } from "@tauri-apps/plugin-process";
import { useEffect, useRef } from "react";
import { formatBytes } from "../lib/format";
import { dismissUpdate, installUpdate, useUpdater } from "../lib/updater";

/** The "new version available" popup: release notes, then download progress. */
export function UpdateDialog() {
  const { status, open } = useUpdater();
  const ref = useRef<HTMLDialogElement>(null);
  const show = open && status.phase !== "idle" && status.phase !== "checking";

  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (show && !d.open) d.showModal();
    if (!show && d.open) d.close();
  }, [show]);

  const update = "update" in status ? status.update : null;
  const working = status.phase === "downloading" || status.phase === "installing";
  const pct =
    status.phase === "downloading" && status.total ? Math.min(100, Math.round((status.done / status.total) * 100)) : null;

  return (
    <dialog
      ref={ref}
      className="dialog dialog-wide"
      aria-labelledby="update-h"
      onCancel={(e) => {
        e.preventDefault();
        dismissUpdate();
      }}
      onKeyDown={(e) => e.stopPropagation()}
    >
      <h2 id="update-h">{status.phase === "error" ? "Update failed" : "Update available"}</h2>
      <div className="dialog-body">
        {update && (
          <p>
            ClipVault <strong>{update.version}</strong> is ready (you have {update.currentVersion}).
          </p>
        )}
        {status.phase === "available" && update?.body && <div className="update-notes">{update.body}</div>}
        {status.phase === "available" && (
          <p className="update-hint">Recording pauses for a moment while the update installs; ClipVault restarts on its own.</p>
        )}
        {status.phase === "downloading" && (
          <div className="progress" aria-live="polite">
            <div className="progress-bar">
              <span style={{ width: `${pct ?? 0}%` }} />
            </div>
            <small>
              Downloading… {formatBytes(status.done)}
              {status.total ? ` of ${formatBytes(status.total)}` : ""}
            </small>
          </div>
        )}
        {status.phase === "installing" && <p aria-live="polite">Installing… ClipVault restarts when it is done.</p>}
        {status.phase === "error" && (
          <p className="field-error" role="alert">
            {status.message}
            {status.captureStopped && " Recording was stopped for the install; restart ClipVault to record again."}
          </p>
        )}
      </div>
      <div className="dialog-actions">
        {status.phase === "error" && status.captureStopped ? (
          <button type="button" className="btn btn-primary" onClick={() => void relaunch()} autoFocus>
            Restart ClipVault
          </button>
        ) : (
          <>
            <button type="button" className="btn" onClick={dismissUpdate} disabled={working} autoFocus>
              {status.phase === "error" ? "Close" : "Later"}
            </button>
            <button type="button" className="btn btn-primary" onClick={() => void installUpdate()} disabled={working}>
              {working ? "Updating…" : status.phase === "error" ? "Try again" : "Update now"}
            </button>
          </>
        )}
      </div>
    </dialog>
  );
}
