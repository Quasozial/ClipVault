import { useEffect, useRef, useState } from "react";
import { api, on, type MovePreview, type MoveProgress, type MoveReport } from "../api";
import { formatBytes } from "../lib/format";

/**
 * Offered after the clip folder changed: moves the library clips from the old
 * folder to the new one (files and library paths; titles, tags and markers
 * stay). Cancel stops after the file being moved.
 */
export function MoveClipsDialog({ from, to, preview, onClose }: { from: string; to: string; preview: MovePreview; onClose(): void }) {
  const ref = useRef<HTMLDialogElement>(null);
  const [progress, setProgress] = useState<MoveProgress | null>(null);
  const [running, setRunning] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [report, setReport] = useState<MoveReport | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    ref.current?.showModal();
  }, []);
  useEffect(() => on("move-progress", setProgress), []);

  const start = async () => {
    setRunning(true);
    setError(null);
    try {
      setReport(await api.moveClips(from, to));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setRunning(false);
    }
  };

  const close = () => {
    if (running) return;
    ref.current?.close();
    onClose();
  };

  const n = (k: number) => `${k} ${k === 1 ? "clip" : "clips"}`;
  const pct = progress && progress.bytes_total > 0 ? (progress.bytes_done / progress.bytes_total) * 100 : 0;

  return (
    <dialog
      ref={ref}
      className="dialog"
      aria-label="Move clips"
      onCancel={(e) => {
        e.preventDefault();
        close();
      }}
      onKeyDown={(e) => e.stopPropagation()}
    >
      <h2>Move your clips?</h2>
      <div className="dialog-body">
        {!running && !report && (
          <>
            <p>
              {n(preview.count)} ({formatBytes(preview.bytes)}) are still in <code>{from}</code>. Move them to <code>{to}</code>?
            </p>
            <p className="hint">
              Titles, tags, markers and trims stay. Other files in the old folder are left where they are. You can keep using ClipVault
              while it runs.
            </p>
            {error && (
              <p className="field-error" role="alert">
                {error}
              </p>
            )}
          </>
        )}
        {running && (
          <div className="progress" aria-live="polite">
            <div className="progress-bar">
              <span style={{ width: `${Math.max(4, pct)}%` }} />
            </div>
            <small>
              {stopping
                ? "Stopping after this clip…"
                : progress
                  ? `Moving ${Math.min(progress.done + 1, progress.total)} of ${progress.total} · ${formatBytes(progress.bytes_done)} of ${formatBytes(progress.bytes_total)}`
                  : "Starting…"}
            </small>
          </div>
        )}
        {report && (
          <>
            <p>
              {n(report.moved)} moved{report.cancelled ? " (cancelled)" : ""}.
              {report.skipped_busy > 0 && ` ${n(report.skipped_busy)} skipped while being compressed; move again later.`}
            </p>
            {report.originals_kept.length > 0 && (
              <p className="hint">
                {report.originals_kept.length} original {report.originals_kept.length === 1 ? "file was" : "files were"} in use and stayed in
                the old folder as well; delete them there once they are closed.
              </p>
            )}
            {report.failed.length > 0 && (
              <ul className="export-failed">
                {report.failed.map((f) => (
                  <li key={f}>{f}</li>
                ))}
              </ul>
            )}
          </>
        )}
      </div>
      <div className="dialog-actions">
        {!running && !report && (
          <>
            <button type="button" className="btn" onClick={close}>
              Keep them there
            </button>
            <button type="button" className="btn btn-primary" onClick={() => void start()} autoFocus>
              Move {n(preview.count)}
            </button>
          </>
        )}
        {running && (
          <button
            type="button"
            className="btn"
            disabled={stopping}
            onClick={() => {
              setStopping(true);
              void api.cancelMoveClips();
            }}
          >
            Cancel
          </button>
        )}
        {report && (
          <button type="button" className="btn btn-primary" onClick={close} autoFocus>
            Done
          </button>
        )}
      </div>
    </dialog>
  );
}
