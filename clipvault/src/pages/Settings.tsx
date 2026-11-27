import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import {
  api,
  isMock,
  on,
  type AudioDevice,
  type DisplayInfo,
  type Encoder,
  type EncoderList,
  type Game,
  type HotkeyAction,
  type HotkeyError,
  type MovePreview,
  type RlStatsInfo,
  type RunningProcess,
  type Settings as SettingsT,
  type StorageInfo,
} from "../api";
import { HotkeyRecorder } from "../components/HotkeyRecorder";
import { MicMeter } from "../components/MicMeter";
import { MoveClipsDialog, MoveStatusPanel } from "../components/MoveClipsDialog";
import { ConfirmDialog, Toggle } from "../components/ui";
import {
  bitrateFor,
  clipSizeEstimate,
  diskShares,
  FRAME_RATES,
  LENGTHS,
  QUALITIES,
  qualityOf,
  RESOLUTIONS,
  withFormat,
  withLength,
  type Quality,
} from "../lib/clipStorage";
import { formatBytes } from "../lib/format";
import { settingsOpenSync } from "../lib/settingsOpen";
import { rebaseSettings } from "../lib/settingsMerge";
import { moveRunning, startImport, useImport } from "../lib/stores";
import { reportError, toast } from "../lib/toast";
import { checkForUpdates, openUpdateDialog, useUpdater } from "../lib/updater";
import { getVersion } from "@tauri-apps/api/app";

const SECTIONS = [
  ["storage", "Clips & storage"],
  ["capture", "Capture"],
  ["audio", "Audio"],
  ["video", "Video"],
  ["hotkeys", "Hotkeys"],
  ["games", "Games"],
  ["rocket-league", "Rocket League"],
  ["general", "General"],
  ["import", "Import"],
] as const;

/** Encoder choices: "Automatic", then what this PC can run, plus the saved
 * choice when this PC cannot run it (so the select never shows blank). */
export function encoderOptions(list: EncoderList | null, current: Encoder): [Encoder, string][] {
  const auto: [Encoder, string] = ["Auto", list ? `Automatic (${list.auto.label})` : "Automatic"];
  const avail: [Encoder, string][] = (list?.available ?? []).map((o) => [o.id, o.label]);
  const known = current === "Auto" || avail.some(([id]) => id === current);
  return [auto, ...avail, ...(known ? [] : [[current, `${current} (not available on this PC)`] as [Encoder, string]])];
}

const HOTKEYS: [HotkeyAction, keyof SettingsT, string, string][] = [
  ["SaveClip", "hotkey_save", "Save clip", "Saves the last seconds of gameplay."],
  ["Mark", "hotkey_mark", "Add marker", "Drops a marker into the next saved clip."],
  ["ToggleDesktop", "hotkey_toggle_desktop", "Desktop capture", "Turns recording the desktop on or off."],
];

function Section(props: { id: string; title: string; intro?: ReactNode; children: ReactNode }) {
  return (
    <section className="s-section" id={`s-${props.id}`} aria-labelledby={`s-${props.id}-h`}>
      <h2 id={`s-${props.id}-h`}>{props.title}</h2>
      {props.intro && <p className="s-intro">{props.intro}</p>}
      <div className="s-rows">{props.children}</div>
    </section>
  );
}

/** Installed version, and a manual "Check for updates" (or the update found). */
function UpdateRow() {
  const { status, upToDateAt } = useUpdater();
  const [version, setVersion] = useState<string | null>(null);
  useEffect(() => {
    if (!isMock) getVersion().then(setVersion, () => setVersion(null));
  }, []);
  const found = "update" in status ? status.update : null;
  const hint = found
    ? `Version ${found.version} is available.`
    : upToDateAt
      ? `Up to date (checked ${upToDateAt.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}).`
      : "Checks for new versions automatically.";
  return (
    <Row label={`Version ${version ?? "–"}`} hint={hint}>
      {found ? (
        <button type="button" className="btn btn-primary" onClick={openUpdateDialog}>
          Update to {found.version}
        </button>
      ) : (
        <button
          type="button"
          className="btn"
          disabled={isMock || status.phase === "checking"}
          onClick={() => void checkForUpdates(true)}
        >
          {status.phase === "checking" ? "Checking…" : "Check for updates"}
        </button>
      )}
    </Row>
  );
}

function Row(props: { label: ReactNode; hint?: ReactNode; children: ReactNode; htmlFor?: string }) {
  return (
    <div className="s-row">
      <div className="s-label">
        <label htmlFor={props.htmlFor}>{props.label}</label>
        {props.hint && <small>{props.hint}</small>}
      </div>
      <div className="s-control">{props.children}</div>
    </div>
  );
}

function displayLabel(d: DisplayInfo, i: number) {
  return `Display ${i + 1}${d.primary ? " (primary)" : ""}, ${d.width}×${d.height} at ${d.refresh_hz} Hz${d.hdr ? ", HDR" : ""}`;
}

// ----- clips & storage -----

function Pick(props: { id: string; label: string; value: string | number; onChange(v: string): void; children: ReactNode }) {
  return (
    <label className="cs-pick" htmlFor={props.id}>
      <span>{props.label}</span>
      <select id={props.id} value={props.value} onChange={(e) => props.onChange(e.target.value)}>
        {props.children}
      </select>
    </label>
  );
}

/** Clip format choices with a size estimate, the clip drive's space and the clip folder. */
function StorageSection(props: { draft: SettingsT; onChange(next: SettingsT): void }) {
  const { draft, onChange } = props;
  const [info, setInfo] = useState<StorageInfo | null>(null);
  const [infoError, setInfoError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);
  const dir = draft.capture_dir;

  const load = useCallback(() => {
    api.storageInfo(dir).then(
      (i) => {
        setInfo(i);
        setInfoError(null);
      },
      (e) => setInfoError(e instanceof Error ? e.message : String(e)),
    );
  }, [dir]);
  useEffect(load, [load]);
  useEffect(() => on("clip-saved", load), [load]);
  useEffect(() => on("clip-updated", load), [load]);
  useEffect(() => on("clips-moved", load), [load]);

  const length = Math.min(draft.save_length_s, draft.max_save_length_s);
  const quality = qualityOf(draft.bitrate_kbps, draft.output_height, draft.fps);
  const estimate = clipSizeEstimate(draft);
  const shares = info ? diskShares(info.total, info.free, info.clips_bytes) : null;
  const pct = (x: number) => `${(x * 100).toFixed(2)}%`;

  const pick = async () => {
    setPicking(true);
    try {
      const folder = await api.pickFolder(dir);
      if (folder) onChange({ ...draft, capture_dir: folder });
    } catch (e) {
      reportError("Could not open the folder picker", e);
    } finally {
      setPicking(false);
    }
  };

  return (
    <section className="s-section" id="s-storage" aria-labelledby="s-storage-h">
      <h2 id="s-storage-h">Clips & storage</h2>
      <p className="s-intro">Higher settings make better-looking clips. Lower settings go easier on your PC and make smaller files.</p>

      <div className="cs-picks">
        <Pick id="cs-len" label="Length" value={length} onChange={(v) => onChange(withLength(draft, Number(v)))}>
          {!LENGTHS.some(([sec]) => sec === length) && <option value={length}>{length} seconds</option>}
          {LENGTHS.map(([sec, l]) => (
            <option key={sec} value={sec}>
              {l}
            </option>
          ))}
        </Pick>
        <Pick
          id="cs-q"
          label="Video quality"
          value={quality ?? "custom"}
          onChange={(v) =>
            v !== "custom" && onChange({ ...draft, bitrate_kbps: bitrateFor(v as Quality, draft.output_height, draft.fps) })
          }
        >
          {QUALITIES.map(([q, l]) => (
            <option key={q} value={q}>
              {l}
            </option>
          ))}
          {!quality && <option value="custom">Custom ({Math.round(draft.bitrate_kbps / 1000)} Mbit/s)</option>}
        </Pick>
        <Pick id="cs-res" label="Resolution" value={draft.output_height} onChange={(v) => onChange(withFormat(draft, Number(v), draft.fps))}>
          {RESOLUTIONS.map((h) => (
            <option key={h} value={h}>
              {h}p{h === 1080 ? " (default)" : ""}
            </option>
          ))}
        </Pick>
        <Pick id="cs-fps" label="FPS" value={draft.fps} onChange={(v) => onChange(withFormat(draft, draft.output_height, Number(v)))}>
          {FRAME_RATES.map((f) => (
            <option key={f} value={f}>
              {f}
              {f === 60 ? " (default)" : ""}
            </option>
          ))}
        </Pick>
      </div>
      <p className="cs-estimate">
        Estimated clip size: <b>up to {formatBytes(estimate)}</b> per clip
        <small>Recording uses only the bitrate a scene needs, so most clips come out smaller.</small>
      </p>

      <h3 className="cs-h">Disk space</h3>
      {infoError && (
        <p className="field-error" role="alert">
          {infoError}
        </p>
      )}
      {!info && !infoError && <p className="muted">Reading disk space…</p>}
      {info && shares && (
        <div className="cs-disk">
          <div className="cs-disk-top">
            <div>
              <b>{formatBytes(info.free)}</b>
              <small>free of {formatBytes(info.total)}</small>
            </div>
            <small>{Math.round(shares.used * 100)}% used</small>
          </div>
          <div
            className="cs-bar"
            role="img"
            aria-label={`ClipVault clips ${formatBytes(info.clips_bytes)}, ${formatBytes(info.free)} free of ${formatBytes(info.total)}`}
          >
            <span className="cs-fill-clips" style={{ width: pct(shares.clips) }} />
            <span className="cs-fill-other" style={{ width: pct(shares.other) }} />
          </div>
          <div className="cs-legend">
            <span>
              <i className="cs-dot cs-fill-clips" /> ClipVault: <b>{formatBytes(info.clips_bytes)}</b> ({info.clip_count}{" "}
              {info.clip_count === 1 ? "clip" : "clips"})
            </span>
            <span>
              <i className="cs-dot cs-fill-other" /> Rest of the PC: <b>{formatBytes(Math.max(0, info.total - info.free - info.clips_bytes))}</b>
            </span>
            <span className="cs-legend-end">
              Room for about <b>{estimate > 0 ? Math.floor(info.free / estimate).toLocaleString() : "–"}</b> more clips
            </span>
          </div>
        </div>
      )}

      <div className="cs-location">
        <div className="cs-location-text">
          <span className="cs-location-label">Clip folder</span>
          <button
            type="button"
            className="cs-path"
            title="Show in Explorer"
            onClick={() => void api.revealInFolder(dir).catch((e) => reportError("Could not open the folder", e))}
          >
            {dir}
          </button>
        </div>
        <button type="button" className="btn" disabled={picking} onClick={() => void pick()}>
          {picking ? "Choosing…" : "Change location"}
        </button>
      </div>
      <p className="cs-location-hint">Takes effect when you save. ClipVault then offers to move your clips from the old folder.</p>
      <MoveStatusPanel />
    </section>
  );
}

// ----- games -----

function GamesSection() {
  const [games, setGames] = useState<Game[] | null>(null);
  const [picker, setPicker] = useState(false);
  const [procs, setProcs] = useState<RunningProcess[] | null>(null);
  const [adding, setAdding] = useState<number | null>(null);
  const dlg = useRef<HTMLDialogElement>(null);

  const load = useCallback(() => {
    api.listGames().then(setGames, (e) => reportError("Could not load games", e));
  }, []);
  useEffect(load, [load]);

  const openPicker = () => {
    setPicker(true);
    setProcs(null);
    dlg.current?.showModal();
    api.listRunningProcesses().then(setProcs, (e) => {
      setProcs([]);
      reportError("Could not list running programs", e);
    });
  };
  const closePicker = () => {
    setPicker(false);
    dlg.current?.close();
  };

  const toggle = async (g: Game, enabled: boolean) => {
    setGames((list) => list?.map((x) => (x.id === g.id ? { ...x, enabled } : x)) ?? null);
    try {
      await api.setGameEnabled(g.id, enabled);
    } catch (e) {
      reportError(`Could not change ${g.name}`, e);
      load();
    }
  };

  const add = async (p: RunningProcess) => {
    setAdding(p.pid);
    try {
      const g = await api.addGameFromProcess(p.pid);
      toast.info(`${g.name} added to your games`);
      closePicker();
      load();
    } catch (e) {
      reportError(`Could not add ${p.exe}`, e);
    } finally {
      setAdding(null);
    }
  };

  return (
    <Section id="games" title="Games" intro="ClipVault records while one of the enabled games is running.">
      <div className="games">
        {games === null && <p className="muted">Loading…</p>}
        {games?.length === 0 && <p className="muted">No games yet. Start a game, then add it from the running programs.</p>}
        {games?.map((g) => (
          <div key={g.id} className="game-row">
            <Toggle
              checked={g.enabled}
              onChange={(v) => void toggle(g, v)}
              label={g.name}
              hint={g.exe_names.join(", ") || g.path || undefined}
            />
          </div>
        ))}
      </div>
      <div>
        <button type="button" className="btn" onClick={openPicker}>
          Add from running program
        </button>
      </div>

      <dialog ref={dlg} className="dialog dialog-wide" onCancel={closePicker} onKeyDown={(e) => e.stopPropagation()}>
        <h2>Add a running program</h2>
        <div className="dialog-body">
          <p className="muted">Programs with an open window. Start the game first if it is not listed.</p>
          {picker && procs === null && <p className="muted">Looking for programs…</p>}
          {procs?.length === 0 && <p className="muted">No programs with a window were found.</p>}
          <ul className="proc-list">
            {procs?.map((p) => (
              <li key={p.pid}>
                <div>
                  <strong>{p.name}</strong>
                  <small title={p.path ?? undefined}>{p.exe}</small>
                </div>
                <button type="button" className="btn btn-small" disabled={adding !== null} onClick={() => void add(p)}>
                  {adding === p.pid ? "Adding…" : "Add"}
                </button>
              </li>
            ))}
          </ul>
        </div>
        <div className="dialog-actions">
          <button type="button" className="btn" onClick={closePicker}>
            Close
          </button>
        </div>
      </dialog>
    </Section>
  );
}

// ----- Rocket League -----

function RocketLeagueSection() {
  const [info, setInfo] = useState<RlStatsInfo | null>(null);
  const [confirm, setConfirm] = useState(false);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);

  const load = useCallback(() => {
    api.rlStatsStatus().then(setInfo, (e) => reportError("Could not read the Rocket League status", e));
  }, []);
  useEffect(load, [load]);

  const enable = async () => {
    setBusy(true);
    try {
      const i = await api.rlEnableStats();
      setInfo(i);
      setNote(i.note);
      setConfirm(false);
    } catch (e) {
      reportError("Could not enable the Stats API", e);
    } finally {
      setBusy(false);
    }
  };

  const statusText =
    info?.status === "enabled" ? "Enabled" : info?.status === "disabled" ? "Disabled" : info ? "Rocket League not found" : "Checking…";

  return (
    <Section
      id="rocket-league"
      title="Rocket League"
      intro="The Stats API lets ClipVault place a marker for every goal. It is a setting inside Rocket League's own files."
    >
      <dl className="rl-facts">
        <div>
          <dt>Stats API</dt>
          <dd className={`rl-status rl-${info?.status ?? "pending"}`}>{statusText}</dd>
        </div>
        <div>
          <dt>Connection</dt>
          <dd>{info ? (info.listener_connected ? "Connected to Rocket League" : "Not connected. Is Rocket League running?") : "–"}</dd>
        </div>
        <div>
          <dt>Port</dt>
          <dd>{info?.port ?? "–"}</dd>
        </div>
        <div>
          <dt>Config file</dt>
          <dd>
            <code>{info?.ini_path ?? "Not found"}</code>
          </dd>
        </div>
      </dl>
      {note && <p className="notice">{note}</p>}
      <div className="row-actions">
        {info?.status === "disabled" && (
          <button type="button" className="btn" onClick={() => setConfirm(true)}>
            Enable Stats API
          </button>
        )}
        <button type="button" className="btn btn-ghost" onClick={load}>
          Refresh
        </button>
      </div>
      <ConfirmDialog
        open={confirm}
        title="Enable the Rocket League Stats API?"
        confirmLabel={busy ? "Enabling…" : "Edit file and enable"}
        busy={busy}
        onConfirm={() => void enable()}
        onCancel={() => setConfirm(false)}
      >
        <p>ClipVault will edit this Rocket League file to turn the Stats API on (port {info?.port}):</p>
        <p>
          <code>{info?.ini_path ?? "DefaultStatsAPI.ini"}</code>
        </p>
        <p>A backup copy is kept next to it. Restart Rocket League afterwards for the change to take effect.</p>
      </ConfirmDialog>
    </Section>
  );
}

// ----- import -----

function ImportSection({ onImported }: { onImported(): void }) {
  const imp = useImport();
  const [available, setAvailable] = useState<boolean | null>(null);
  const lastGen = useRef(imp.generation);

  useEffect(() => {
    api.momentsDbAvailable().then(setAvailable, (e) => reportError("Could not check for a Moments library", e));
  }, []);

  useEffect(() => {
    if (imp.generation !== lastGen.current) {
      lastGen.current = imp.generation;
      onImported();
    }
  }, [imp.generation, onImported]);

  const r = imp.report;
  return (
    <Section
      id="import"
      title="Import from SteelSeries Moments"
      intro="Adds your Moments clips with their titles, tags, trims and markers. The files stay where they are. Running it again only adds clips that are new since last time."
    >
      <div className="row-actions">
        <button type="button" className="btn" disabled={imp.running || available === false} onClick={() => void startImport()}>
          {imp.running ? "Importing…" : "Import from Moments"}
        </button>
        {available === false && <span className="hint">No Moments library was found on this PC.</span>}
      </div>
      {imp.running && (
        <div className="progress" aria-live="polite">
          <div className="progress-bar">
            <span style={{ width: imp.progress?.total ? `${(imp.progress.done / imp.progress.total) * 100}%` : "4%" }} />
          </div>
          <small>{imp.progress ? `Importing ${imp.progress.done} of ${imp.progress.total}` : "Reading the Moments library…"}</small>
        </div>
      )}
      {imp.error && !imp.running && (
        <p className="field-error" role="alert">
          Import failed: {imp.error}
        </p>
      )}
      {r && !imp.running && (
        <div className="report">
          <p>
            <b>{r.imported}</b> {r.imported === 1 ? "clip" : "clips"} imported, <b>{r.skipped_existing}</b> already in your
            library, <b>{r.games_imported}</b> {r.games_imported === 1 ? "game" : "games"} added. Thumbnails are made as
            you scroll the library.
          </p>
          {r.missing_files.length > 0 && (
            <details>
              <summary>
                {r.missing_files.length} {r.missing_files.length === 1 ? "file was" : "files were"} missing and not imported
              </summary>
              <ul className="missing">
                {r.missing_files.map((f) => (
                  <li key={f}>
                    <code>{f}</code>
                  </li>
                ))}
              </ul>
            </details>
          )}
        </div>
      )}
    </Section>
  );
}

// ----- page -----

export function Settings({ section }: { section?: string }) {
  const [saved, setSaved] = useState<SettingsT | null>(null);
  const [draft, setDraft] = useState<SettingsT | null>(null);
  const [displays, setDisplays] = useState<DisplayInfo[]>([]);
  const [devices, setDevices] = useState<AudioDevice[]>([]);
  const [encoders, setEncoders] = useState<EncoderList | null>(null);
  const [hotkeyErrors, setHotkeyErrors] = useState<HotkeyError[]>([]);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  /** Offer to move the clips left in the old clip folder after it changed. */
  const [move, setMove] = useState<{ from: string; to: string; preview: MovePreview } | null>(null);
  const draftRef = useRef(draft);
  draftRef.current = draft;
  const savedRef = useRef(saved);
  savedRef.current = saved;

  // Takes the backend's current settings: fields the user edited keep the
  // user's value, all others follow the backend (tray / hotkey desktop toggle,
  // Moments import, another save).
  const fetchSettings = useCallback(() => {
    api.getSettings().then(
      (latest) => {
        const draft = draftRef.current;
        const base = savedRef.current;
        setSaved(latest);
        setDraft(draft && base ? rebaseSettings(draft, base, latest) : latest);
      },
      (e) => reportError("Could not load settings", e),
    );
  }, []);

  useEffect(() => on("settings-changed", fetchSettings), [fetchSettings]);

  useEffect(() => {
    fetchSettings();
    api.listDisplays().then(setDisplays, (e) => reportError("Could not list displays", e));
    api.listEncoders().then(setEncoders, (e) => reportError("Could not list video encoders", e));
    api.listAudioDevices().then(setDevices, (e) => reportError("Could not list audio devices", e));
    api.getHotkeyErrors().then(setHotkeyErrors, (e) => reportError("Could not read hotkey status", e));
  }, [fetchSettings]);

  // The backend runs the idle mic meter (microphone in use) only while this page
  // is open AND visible. Closing the window only hides it to the tray, and
  // WebView2 does not fire `visibilitychange` for that (checked in the real
  // app), so the backend sends `window-visibility`; `visibilitychange` still
  // covers minimising.
  useEffect(() => {
    const sync = settingsOpenSync((open) =>
      api
        .setSettingsOpen(open)
        .catch((e) => reportError(open ? "Could not start the mic meter" : "Could not stop the mic meter", e)),
    );
    let windowHidden = false;
    const refresh = () => sync.update(document.hidden || windowHidden);
    refresh();
    document.addEventListener("visibilitychange", refresh);
    const off = on("window-visibility", ({ visible }) => {
      windowHidden = !visible;
      refresh();
    });
    return () => {
      document.removeEventListener("visibilitychange", refresh);
      off();
      sync.dispose();
    };
  }, []);

  useEffect(() => {
    if (section && draft) document.getElementById(`s-${section}`)?.scrollIntoView({ block: "start" });
  }, [section, Boolean(draft)]); // eslint-disable-line react-hooks/exhaustive-deps

  // Marks the section being read in the nav: the last one whose heading has
  // passed the top third of the scroller (the final one once scrolled to the end).
  const scroller = useRef<HTMLDivElement>(null);
  const [active, setActive] = useState<string>(section ?? SECTIONS[0][0]);
  // A section picked in the nav stays marked until the user scrolls by hand
  // (near the end, short sections cannot reach the top).
  const picked = useRef<string | null>(section ?? null);
  const loaded = Boolean(draft);
  useEffect(() => {
    const el = scroller.current;
    if (!el) return;
    const update = () => {
      if (picked.current) return setActive(picked.current);
      const top = el.getBoundingClientRect().top + el.clientHeight / 3;
      let cur: string = SECTIONS[0][0];
      for (const [id] of SECTIONS) {
        const s = document.getElementById(`s-${id}`);
        if (s && s.getBoundingClientRect().top <= top) cur = id;
      }
      if (el.scrollTop + el.clientHeight >= el.scrollHeight - 2) cur = SECTIONS[SECTIONS.length - 1][0];
      setActive(cur);
    };
    const manual = () => {
      picked.current = null;
    };
    update();
    el.addEventListener("scroll", update, { passive: true });
    el.addEventListener("wheel", manual, { passive: true });
    el.addEventListener("touchstart", manual, { passive: true });
    el.addEventListener("keydown", manual);
    el.addEventListener("pointerdown", manual);
    return () => {
      el.removeEventListener("scroll", update);
      el.removeEventListener("wheel", manual);
      el.removeEventListener("touchstart", manual);
      el.removeEventListener("keydown", manual);
      el.removeEventListener("pointerdown", manual);
    };
  }, [loaded]);

  if (!draft || !saved) {
    return <div className="settings settings-loading muted">Loading settings…</div>;
  }

  const set = <K extends keyof SettingsT>(k: K, v: SettingsT[K]) => {
    setDraft((d) => (d ? { ...d, [k]: v } : d));
    setSaveError(null);
  };
  const dirty = JSON.stringify(draft) !== JSON.stringify(saved);

  const save = async () => {
    setSaving(true);
    try {
      // Send only what the user changed, on top of the backend's settings as
      // they are now: a stale draft must not undo a tray toggle or an import.
      const sent = rebaseSettings(draft, saved, await api.getSettings());
      const errs = await api.saveSettings(sent);
      setHotkeyErrors(errs);
      const after = await api.getSettings();
      // Edits made while saving (draft moved on from what this save started
      // with) stay; everything else shows what the backend now has.
      setSaved(after);
      setDraft(rebaseSettings(draftRef.current ?? draft, draft, after));
      setSaveError(null);
      toast.info(errs.length ? "Settings saved, but some hotkeys could not be registered" : "Settings saved");
      const [from, to] = [saved.capture_dir, after.capture_dir];
      if (from.trim().toLowerCase() !== to.trim().toLowerCase() && !moveRunning()) {
        api.moveClipsPreview(from).then(
          (preview) => preview.count > 0 && setMove({ from, to, preview }),
          (e) => reportError("Could not check the old clip folder", e),
        );
      }
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  const inputs = devices.filter((d) => d.is_input);
  const outputs = devices.filter((d) => !d.is_input);
  const monitor = displays.find((d) => d.id === draft.monitor_id) ?? displays.find((d) => d.primary);
  const hkError = (a: HotkeyAction) => {
    const e = hotkeyErrors.find((x) => x.action === a);
    return e ? `${e.accelerator} could not be registered: ${e.reason}` : undefined;
  };

  return (
    <div className="settings" ref={scroller}>
      {move && <MoveClipsDialog {...move} onClose={() => setMove(null)} />}
      <nav className="s-nav" aria-label="Settings sections">
        {SECTIONS.map(([id, label]) => (
          <a
            key={id}
            href={`#/settings/${id}`}
            aria-current={active === id ? "location" : undefined}
            onClick={(e) => {
              e.preventDefault();
              picked.current = id;
              setActive(id);
              document.getElementById(`s-${id}`)?.scrollIntoView({ behavior: "smooth", block: "start" });
            }}
          >
            {label}
          </a>
        ))}
      </nav>

      <div className="s-body">
        <StorageSection
          draft={draft}
          onChange={(next) => {
            setDraft(next);
            setSaveError(null);
          }}
        />
        <Section id="capture" title="Capture">
          <Row label="Longest clip" hint="Upper limit for the clip length (set under Clips & storage); up to 600 seconds." htmlFor="maxlen">
            <div className="with-unit">
              <input
                id="maxlen"
                type="number"
                min={5}
                max={600}
                value={draft.max_save_length_s}
                onChange={(e) => set("max_save_length_s", Number(e.target.value))}
              />
              <span>s</span>
            </div>
          </Row>
          <Row
            label="Encoder"
            hint={
              encoders && !encoders.auto.hardware
                ? "No graphics card encoder found: recording uses the CPU. Lower resolution or frame rate if games stutter."
                : "Only encoders that work on this PC are listed. Automatic picks the best one."
            }
            htmlFor="enc"
          >
            <select id="enc" value={draft.encoder} onChange={(e) => set("encoder", e.target.value as Encoder)}>
              {encoderOptions(encoders, draft.encoder).map(([v, l]) => (
                <option key={v} value={v}>
                  {l}
                </option>
              ))}
            </select>
          </Row>
          <Row
            label="Max bitrate"
            hint="2 to 150 Mbit/s. Recording is quality-based and stays below this; quiet scenes use far less."
            htmlFor="br"
          >
            <div className="with-unit">
              <input
                id="br"
                type="number"
                min={2}
                max={150}
                step={1}
                value={Math.round(draft.bitrate_kbps / 1000)}
                onChange={(e) => set("bitrate_kbps", Math.round(Number(e.target.value) * 1000))}
              />
              <span>Mbit/s</span>
            </div>
          </Row>
          <Row label="Monitor" htmlFor="mon" hint="Desktop recording, and games when Game capture is Whole monitor.">
            <div className="inline">
              <select id="mon" value={draft.monitor_id ?? ""} onChange={(e) => set("monitor_id", e.target.value || null)}>
                <option value="">Primary display</option>
                {displays.map((d, i) => (
                  <option key={d.id} value={d.id}>
                    {displayLabel(d, i)}
                  </option>
                ))}
              </select>
              {monitor?.hdr && (
                <span className="badge" title="HDR is on for this display">
                  HDR
                </span>
              )}
            </div>
          </Row>
          <Row
            label="Game capture"
            htmlFor="gcap"
            hint="Game window records only the game: no desktop, no pop-ups, black while it is minimized. Whole monitor records everything on the game's screen."
          >
            <select id="gcap" value={draft.game_capture} onChange={(e) => set("game_capture", e.target.value as "Window" | "Monitor")}>
              <option value="Window">Game window (like OBS)</option>
              <option value="Monitor">Whole monitor</option>
            </select>
          </Row>
          <Row label="Desktop capture" hint="Record the desktop when no game is running. Also on the hotkey and the tray menu.">
            <Toggle checked={draft.desktop_capture} onChange={(v) => set("desktop_capture", v)} label={draft.desktop_capture ? "On" : "Off"} />
          </Row>
          <Row
            label="Compress saved clips"
            hint="Re-encodes clips to AV1 in the background, usually a third of the size or less. Waits while a game is recording."
          >
            <Toggle checked={draft.compress_clips} onChange={(v) => set("compress_clips", v)} label={draft.compress_clips ? "On" : "Off"} />
          </Row>
          <Row label="Buffer folder" hint="Rolling recording before a save, kept in a ClipVault-buffer subfolder. A fast drive helps." htmlFor="bdir">
            <input id="bdir" className="path-input" value={draft.buffer_dir} onChange={(e) => set("buffer_dir", e.target.value)} spellCheck={false} />
          </Row>
        </Section>

        <Section id="audio" title="Audio">
          <Row label="Game audio from" htmlFor="out">
            <select id="out" value={draft.output_device ?? ""} onChange={(e) => set("output_device", e.target.value || null)}>
              <option value="">Default output</option>
              {outputs.map((d) => (
                <option key={d.id} value={d.id}>
                  {d.name}
                  {d.is_default ? " (default)" : ""}
                </option>
              ))}
            </select>
          </Row>
          <Row label="Microphone" htmlFor="mic">
            <select id="mic" value={draft.mic_device ?? ""} onChange={(e) => set("mic_device", e.target.value || null)} disabled={!draft.mic_enabled}>
              <option value="">Default microphone</option>
              {inputs.map((d) => (
                <option key={d.id} value={d.id}>
                  {d.name}
                  {d.is_default ? " (default)" : ""}
                </option>
              ))}
            </select>
          </Row>
          <Row label="Record microphone">
            <Toggle checked={draft.mic_enabled} onChange={(v) => set("mic_enabled", v)} label={draft.mic_enabled ? "On" : "Off"} />
          </Row>
          <Row label="Discord on its own track" hint="Keeps voice chat separate from game sound in saved clips.">
            <Toggle checked={draft.discord_split} onChange={(v) => set("discord_split", v)} label={draft.discord_split ? "On" : "Off"} />
          </Row>
          <Row label="Laugh markers" hint="Marks moments when the mic gets louder than the threshold.">
            <Toggle checked={draft.laugh_enabled} onChange={(v) => set("laugh_enabled", v)} label={draft.laugh_enabled ? "On" : "Off"} />
          </Row>
          <Row label="Laugh threshold" hint="Drag the line. Talk normally, then laugh: set it between the two.">
            <MicMeter
              threshold={Math.round(draft.laugh_threshold_dbfs)}
              onThreshold={(v) => set("laugh_threshold_dbfs", v)}
              disabled={!draft.mic_enabled}
            />
          </Row>
        </Section>

        <Section id="video" title="Video">
          <Row label="HDR" hint="Read from Windows for the recorded monitor.">
            <span className="value">
              {monitor ? (
                monitor.hdr ? (
                  <>
                    <span className="badge">HDR</span> On for {monitor.width}×{monitor.height}
                  </>
                ) : (
                  "Off"
                )
              ) : (
                "Unknown"
              )}
            </span>
          </Row>
        </Section>

        <Section id="hotkeys" title="Hotkeys" intro="Click a shortcut, then press the new keys. They work while a game has focus.">
          {HOTKEYS.map(([action, key, label, hint]) => (
            <Row key={action} label={label} hint={hint}>
              <HotkeyRecorder
                label={label}
                value={draft[key] as string}
                onChange={(v) => {
                  set(key, v as never);
                  setHotkeyErrors((errs) => errs.filter((x) => x.action !== action));
                }}
                error={hkError(action)}
              />
            </Row>
          ))}
        </Section>

        <GamesSection />
        <RocketLeagueSection />

        <Section id="general" title="General">
          <Row label="Start with Windows" hint="Starts in the tray when you sign in.">
            <Toggle checked={draft.start_with_windows} onChange={(v) => set("start_with_windows", v)} label={draft.start_with_windows ? "On" : "Off"} />
          </Row>
          <UpdateRow />
          <Row label="Log files" hint="Send these when reporting a problem.">
            <button
              type="button"
              className="btn"
              onClick={() => void api.openLogsFolder().catch((e) => reportError("Could not open the log folder", e))}
            >
              Open logs folder
            </button>
          </Row>
        </Section>

        <ImportSection onImported={fetchSettings} />
      </div>

      {(dirty || saveError) && (
        <div className={`savebar${saveError ? " has-error" : ""}`} role="region" aria-label="Unsaved changes">
          {saveError ? (
            <span className="field-error" role="alert">
              Not saved: {saveError}
            </span>
          ) : (
            <span>You have unsaved changes.</span>
          )}
          <span className="savebar-actions">
            <button
              type="button"
              className="btn btn-ghost"
              disabled={saving}
              onClick={() => {
                setDraft(saved);
                setSaveError(null);
              }}
            >
              Discard
            </button>
            <button type="button" className="btn btn-primary" disabled={saving || !dirty} onClick={() => void save()}>
              {saving ? "Saving…" : "Save changes"}
            </button>
          </span>
        </div>
      )}
    </div>
  );
}
