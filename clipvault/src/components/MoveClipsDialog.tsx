import { useEffect, useRef, useState } from "react";
import { api, type MovePreview, type MoveState } from "../api";
import { formatBytes } from "../lib/format";
import { dismissMove, startMove, useMove } from "../lib/stores";

const n = (k: number) => `${k} ${k === 1 ? "clip" : "clips"}`;

/** Progress bar, status line and result of a clip move (dialog and Settings). */
export function MoveStatusView({ move, stopping }: { move: MoveState; stopping?: boolean }) {
  const p = move.progress;
  const pct = p && p.bytes_total > 0 ? (p.bytes_done / p.bytes_total) * 100 : 0;
  if (move.running) {
    return (
      <div className="progress" aria-live="polite">
        <div className="progress-bar">
          <span style={{ width: `${Math.max(4, pct)}%` }} />
        </div>
        <small>
          {stopping
            ? "Stopping after this clip…"
            : p
              ? `Moving ${Math.min(p.done + 1, p.total)} of ${p.total} · ${formatBytes(p.bytes_done)} of ${formatBytes(p.bytes_total)} · ${Math.floor(pct)}%`
              : "Starting…"}
        </small>
      </div>
    );
  }
  if (move.error) {
    return (
      <p className="field-error" role="alert">
        Moving clips failed: {move.error}
      </p>
    );
  }
  const r = move.report;
  if (!r) return null;
  return (
    <>
      <p>
        {n(r.moved)} moved to <code>{move.to}</code>
        {r.cancelled ? " (cancelled)" : ""}.
        {r.skipped_busy > 0 && ` ${n(r.skipped_busy)} skipped while being compressed; move again later.`}
      </p>
      {r.originals_kept.length > 0 && (
        <p className="hint">
          {r.originals_kept.length} original {r.originals_kept.length === 1 ? "file was" : "files were"} in use and stayed in the old
          folder as well; delete them there once they are closed.
        </p>
      )}
      {r.failed.length > 0 && (
        <ul className="export-failed">
          {r.failed.map((f) => (
            <li key={f}>{f}</li>
          ))}
        </ul>
      )}
    </>
  );
}

/** Clip move progress under the Clip folder setting; shown while a move runs and after it until dismissed. */
export function MoveStatusPanel() {
  const move = useMove();
  const [stopping, setStopping] = useState(false);
  useEffect(() => {
    if (!move?.running) setStopping(false);
  }, [move?.running]);
  if (!move) return null;
  return (
    <div className="move-status">
      <MoveStatusView move={move} stopping={stopping} />
      <div className="move-status-actions">
        {move.running ? (
          <button
            type="button"
            className="btn"
            disabled={stopping}
            onClick={() => {
              setStopping(true);
              void api.cancelMoveClips();
            }}
          >
            Cancel move
          </button>
        ) : (
          <button type="button" className="btn" onClick={dismissMove}>
            Dismiss
          </button>
        )}
      </div>
    </div>
  );
}

/**
 * Offered after the clip folder changed: moves the library clips from the old
 * folder to the new one (files and library paths; titles, tags and markers
 * stay). The move runs in the background: the dialog can be hidden and the
 * Clip folder setting keeps showing its progress.
 */
export function MoveClipsDialog({ from, to, preview, onClose }: { from: string; to: string; preview: MovePreview; onClose(): void }) {
  const ref = useRef<HTMLDialogElement>(null);
  const move = useMove();
  const [started, setStarted] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    ref.current?.showModal();
  }, []);

  const start = async () => {
    setError(null);
    try {
      await startMove(from, to);
      setStarted(true);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const close = () => {
    ref.current?.close();
    onClose();
  };

  const running = started && move?.running;
  const finished = started && move && !move.running;

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
        {!started && (
          <>
            <p>
              {n(preview.count)} ({formatBytes(preview.bytes)}) are still in <code>{from}</code>. Move them to <code>{to}</code>?
            </p>
            <p className="hint">
              Titles, tags, markers and trims stay. Other files in the old folder are left where they are. The move runs in the
              background; its progress stays visible under Clip folder.
            </p>
            {error && (
              <p className="field-error" role="alert">
                {error}
              </p>
            )}
          </>
        )}
        {started && move && <MoveStatusView move={move} stopping={stopping} />}
      </div>
      <div className="dialog-actions">
        {!started && (
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
          <>
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
            <button type="button" className="btn btn-primary" onClick={close} autoFocus>
              Hide
            </button>
          </>
        )}
        {finished && (
          <button
            type="button"
            className="btn btn-primary"
            onClick={() => {
              dismissMove();
              close();
            }}
            autoFocus
          >
            Done
          </button>
        )}
      </div>
    </dialog>
  );
}
