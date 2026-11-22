import { useEffect, useRef, useState } from "react";
import { api, type Clip } from "../api";
import { allTracksMixed, EXPORT_PRESETS, exportOptions } from "../lib/player";
import { runBulk, type BulkResult } from "../lib/selection";
import { reportError } from "../lib/toast";

/**
 * Exports several clips one after another with one preset. Every audio track
 * is mixed in at full volume (the per-clip track sliders live in the clip
 * view). Cancel stops after the clip being exported.
 */
export function BulkExportDialog({ clips, onClose }: { clips: Clip[]; onClose(): void }) {
  const ref = useRef<HTMLDialogElement>(null);
  const [presetId, setPresetId] = useState(EXPORT_PRESETS[0].id);
  const [done, setDone] = useState(0);
  const [running, setRunning] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [result, setResult] = useState<BulkResult | null>(null);
  const [lastPath, setLastPath] = useState<string | null>(null);
  const abort = useRef<AbortController | null>(null);

  useEffect(() => {
    ref.current?.showModal();
  }, []);

  const preset = EXPORT_PRESETS.find((p) => p.id === presetId) ?? EXPORT_PRESETS[0];
  const titleOf = (id: string) => clips.find((c) => c.id === id)?.title ?? id;

  const start = async () => {
    const ctl = new AbortController();
    abort.current = ctl;
    setRunning(true);
    setDone(0);
    const r = await runBulk(
      clips.map((c) => c.id),
      async (id) => {
        const tracks = await api.audioTracks(id);
        const path = await api.exportClip(id, exportOptions(preset, allTracksMixed(tracks)));
        setLastPath(path);
      },
      { signal: ctl.signal, onProgress: setDone },
    );
    setRunning(false);
    setResult(r);
  };

  const close = () => {
    if (running) return;
    ref.current?.close();
    onClose();
  };

  const cancelled = result !== null && result.ok.length + result.failed.length < clips.length;

  return (
    <dialog
      ref={ref}
      className="dialog"
      aria-label="Export clips"
      onCancel={(e) => {
        e.preventDefault();
        close();
      }}
      onKeyDown={(e) => e.stopPropagation()}
    >
      <h2>
        Export {clips.length} {clips.length === 1 ? "clip" : "clips"}
      </h2>
      <div className="dialog-body">
        {!running && !result && (
          <>
            <label className="field">
              <span>Preset</span>
              <select value={presetId} onChange={(e) => setPresetId(e.target.value)}>
                {EXPORT_PRESETS.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.name}
                  </option>
                ))}
              </select>
            </label>
            <p className="hint">{preset.hint} All audio tracks are mixed in at full volume. Files go to the Exports folder.</p>
          </>
        )}
        {running && (
          <div className="progress" aria-live="polite">
            <div className="progress-bar">
              <span style={{ width: `${Math.max(4, (done / clips.length) * 100)}%` }} />
            </div>
            <small>
              {stopping ? "Stopping after this clip…" : `Exporting ${Math.min(done + 1, clips.length)} of ${clips.length}`}
            </small>
          </div>
        )}
        {result && (
          <>
            <p>
              {result.ok.length} of {clips.length} exported{cancelled ? " (cancelled)" : ""}.
            </p>
            {result.failed.length > 0 && (
              <ul className="export-failed">
                {result.failed.map((f) => (
                  <li key={f.id}>
                    <strong>{titleOf(f.id)}</strong>: {f.error}
                  </li>
                ))}
              </ul>
            )}
          </>
        )}
      </div>
      <div className="dialog-actions">
        {!running && !result && (
          <>
            <button type="button" className="btn" onClick={close}>
              Cancel
            </button>
            <button type="button" className="btn btn-primary" onClick={() => void start()} autoFocus>
              Export
            </button>
          </>
        )}
        {running && (
          <button type="button" className="btn" disabled={stopping}
            onClick={() => {
              abort.current?.abort();
              setStopping(true);
            }}>
            Cancel
          </button>
        )}
        {result && (
          <>
            {lastPath && (
              <button
                type="button"
                className="btn"
                onClick={() => void api.revealInFolder(lastPath).catch((e) => reportError("Could not open the folder", e))}
              >
                Show in folder
              </button>
            )}
            <button type="button" className="btn btn-primary" onClick={close} autoFocus>
              Done
            </button>
          </>
        )}
      </div>
    </dialog>
  );
}
