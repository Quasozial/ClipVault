import { useCallback, useEffect, useRef, useState } from "react";
import { api, fileSrc, on, type AudioTrack, type Clip, type CompressStage } from "../api";
import { StarIcon } from "../components/StarIcon";
import { forgetThumb } from "../components/Thumb";
import { Timeline, type TimelineTrack } from "../components/Timeline";
import { ConfirmDialog } from "../components/ui";
import { formatBytes, formatClock, formatDuration, formatTakenAt } from "../lib/format";
import { addTags } from "../lib/tags";
import { navigate } from "../lib/stores";
import { EXPORT_PRESETS, exportOptions, exportTracks, formatSpeed, frameStep, markerJump, SPEEDS, stepSpeed } from "../lib/player";
import { reportError, toast } from "../lib/toast";

type Meta = { title: string; description: string; tags: string[] };

const metaOf = (c: Clip): Meta => ({ title: c.title, description: c.description, tags: c.tags });

function sameMeta(a: Meta, b: Meta): boolean {
  return a.title === b.title && a.description === b.description && a.tags.join("\u0000") === b.tags.join("\u0000");
}

const VOL_KEY = "clipvault.trackVolume.";
/** Re-sync a track when it drifts further than this from the video (s). */
const MAX_DRIFT = 0.12;

function loadVolume(kind: string): number {
  try {
    const v = Number(localStorage.getItem(VOL_KEY + kind));
    return localStorage.getItem(VOL_KEY + kind) !== null && Number.isFinite(v) ? Math.min(1, Math.max(0, v)) : 1;
  } catch {
    return 1;
  }
}

function saveVolume(kind: string, v: number) {
  try {
    localStorage.setItem(VOL_KEY + kind, String(v));
  } catch {
    /* per-viewer convenience only */
  }
}

const trackKey = (t: AudioTrack) => `${t.kind}-${t.index}`;

const fileName = (p: string) => p.split(/[\\/]/).pop() ?? p;

const EDGE = 0.05; // a trim this close to the clip's start/end means "no trim" on that side

function isTyping(t: EventTarget | null) {
  return (
    t instanceof HTMLInputElement ||
    t instanceof HTMLTextAreaElement ||
    t instanceof HTMLSelectElement ||
    (t instanceof HTMLElement && t.isContentEditable)
  );
}

function TagEditor({ tags, onChange }: { tags: string[]; onChange(tags: string[]): void }) {
  const [draft, setDraft] = useState("");
  // Adds what `text` completes (all of it when `all`), keeps the rest in the field.
  const apply = (text: string, all: boolean) => {
    const r = addTags(tags, text, all);
    setDraft(r.rest);
    if (r.tags.length !== tags.length) onChange(r.tags);
  };
  const commit = () => apply(draft, true);
  return (
    <div className="tags" onClick={(e) => (e.currentTarget.querySelector("input") as HTMLInputElement | null)?.focus()}>
      {tags.map((t) => (
        <span key={t} className="chip">
          {t}
          <button type="button" aria-label={`Remove tag ${t}`} onClick={() => onChange(tags.filter((x) => x !== t))}>
            <svg width="9" height="9" viewBox="0 0 12 12" aria-hidden>
              <path d="M2 2l8 8M10 2l-8 8" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />
            </svg>
          </button>
        </span>
      ))}
      <input
        value={draft}
        placeholder={tags.length ? "Add tag" : "Add tags, Enter to add"}
        aria-label="Add tag"
        onChange={(e) => {
          const v = e.target.value;
          if (v.includes(",")) apply(v, false);
          else setDraft(v);
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            commit();
          } else if (e.key === "Backspace" && !draft && tags.length) {
            onChange(tags.slice(0, -1));
          }
        }}
        onBlur={commit}
      />
    </div>
  );
}

export function ClipView({ id }: { id: string }) {
  const [clip, setClip] = useState<Clip | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [title, setTitle] = useState("");
  const [description, setDescription] = useState("");
  const [titleError, setTitleError] = useState<string | null>(null);
  const [current, setCurrent] = useState(0);
  const [mediaDuration, setMediaDuration] = useState<number | null>(null);
  const [playing, setPlaying] = useState(false);
  const [videoError, setVideoError] = useState(false);
  const [accurate, setAccurate] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [presetId, setPresetId] = useState(EXPORT_PRESETS[0].id);
  const [muted, setMuted] = useState<Record<string, boolean>>({});
  const [sharing, setSharing] = useState(false);
  const [shared, setShared] = useState<string | null>(null);
  const [rate, setRate] = useState(1);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const videoRef = useRef<HTMLVideoElement>(null);
  const playerRef = useRef<HTMLDivElement>(null);
  // Every audio track plays from its own element (a <video> only plays the
  // first track); the video itself is muted while they work.
  const [tracks, setTracks] = useState<AudioTrack[] | undefined>(undefined);
  const [tracksError, setTracksError] = useState<string | null>(null);
  const [volumes, setVolumes] = useState<Record<string, number>>({});
  const [audioFailed, setAudioFailed] = useState(false);
  const audioEls = useRef(new Map<string, HTMLAudioElement>());
  const separateAudio = Boolean(tracks && tracks.length > 0 && !audioFailed);
  const separate = useRef(separateAudio);
  separate.current = separateAudio;

  const [tags, setTags] = useState<string[]>([]);
  // Metadata the user last committed (`wanted`) and the last the backend
  // accepted (`saved`). Saves run one at a time and always send all of
  // `wanted`, so a slow save can never revert a newer edit.
  const wanted = useRef<Meta | null>(null);
  const saved = useRef<Meta | null>(null);
  const saveChain = useRef<Promise<void>>(Promise.resolve());
  // Esc in the title field: the blur that follows must not save.
  const titleCancelled = useRef(false);

  useEffect(() => {
    let stale = false;
    setClip(null);
    setLoadError(null);
    wanted.current = saved.current = null;
    api.getClip(id).then(
      (c) => {
        if (stale) return;
        wanted.current = saved.current = metaOf(c);
        loadedSize.current = c.size;
        setClip(c);
        setTitle(c.title);
        setDescription(c.description);
        setTags(c.tags);
      },
      (e) => {
        if (stale) return;
        setLoadError(e.message);
        reportError("Could not open the clip", e);
      },
    );
    return () => {
      stale = true;
    };
  }, [id]);

  // Background compression finished: new size, and the Compress button goes.
  // The stage lives in the backend, so it is still known when the clip is
  // opened again; `asked` covers the moment until the backend answers.
  const [compressStage, setCompressStage] = useState<CompressStage | "asked" | null>(null);
  // The compressed file cannot replace one the player holds open: at
  // `swapping` the video lets go of it, and once the stage ends a fresh
  // element (new `videoVersion`) loads the file again where it was.
  const [videoVersion, setVideoVersion] = useState(0);
  const resume = useRef<{ t: number; play: boolean } | null>(null);
  // Size of the file the player has loaded: a new size means the file was
  // replaced under it.
  const loadedSize = useRef<number | null>(null);
  useEffect(() => {
    let live = true;
    let heard = false;
    let released = false;
    setCompressStage(null);
    setVideoVersion(0);
    setVideoError(false);
    resume.current = null;
    loadedSize.current = null;
    // A fresh element loads the file again where the old one was.
    const reload = () => {
      const v = videoRef.current;
      if (v && !resume.current) resume.current = { t: v.currentTime, play: !v.paused };
      setVideoError(false);
      setVideoVersion((n) => n + 1);
    };
    const offClip = on("clip-updated", (c) => {
      if (c.id !== id) return;
      setClip((old) => (old ? { ...old, size: c.size, compressed: c.compressed } : c));
      // The swap usually succeeds while the player still has the file open
      // (no `swapping` stage): the old element would read the new file with
      // the old one's layout and fail.
      const before = loadedSize.current;
      loadedSize.current = c.size;
      if (before !== null && before !== c.size && !released) reload();
    });
    const offStage = on("compress-status", (s) => {
      if (s.id !== id) return;
      heard = true;
      setCompressStage(s.stage);
      const v = videoRef.current;
      if (s.stage === "swapping" && !released && v) {
        released = true;
        resume.current = { t: v.currentTime, play: !v.paused };
        v.pause();
        v.removeAttribute("src");
        v.load();
      } else if (s.stage === null && released) {
        released = false;
        reload();
      }
    });
    api.compressStatus(id).then(
      // An event that arrived meanwhile is newer than this answer.
      (stage) => live && !heard && setCompressStage(stage),
      () => {},
    );
    return () => {
      live = false;
      offClip();
      offStage();
    };
  }, [id]);

  useEffect(() => {
    let live = true;
    setTracks(undefined);
    setTracksError(null);
    setAudioFailed(false);
    api.audioTracks(id).then(
      (t) => {
        if (!live) return;
        setVolumes(Object.fromEntries(t.map((x) => [trackKey(x), loadVolume(x.kind)])));
        setTracks(t);
      },
      (e: Error) => live && setTracksError(e.message),
    );
    return () => {
      live = false;
    };
  }, [id]);

  useEffect(() => {
    for (const [k, a] of audioEls.current) a.volume = volumes[k] ?? 1;
  }, [volumes, tracks]);

  // One speed for the video and every separate audio track (pitch preserved).
  useEffect(() => {
    if (videoRef.current) videoRef.current.playbackRate = rate;
    for (const a of audioEls.current.values()) a.playbackRate = rate;
  }, [rate, tracks, clip]);

  /** Puts every track at the video's position and plays or pauses it. */
  const syncAudio = useCallback((play: boolean) => {
    const v = videoRef.current;
    if (!v || !separate.current) return;
    for (const a of audioEls.current.values()) {
      if (Math.abs(a.currentTime - v.currentTime) > 0.02) a.currentTime = v.currentTime;
      if (play) a.play().catch(() => {});
      else a.pause();
    }
  }, []);

  useEffect(() => {
    if (audioFailed) for (const a of audioEls.current.values()) a.pause();
  }, [audioFailed]);

  const timelineTracks: TimelineTrack[] | undefined = tracks?.map((t) => ({
    key: trackKey(t),
    label: t.label,
    kind: t.kind,
    peaks: t.peaks,
    peaksPerSec: t.peaks_per_sec,
    volume: volumes[trackKey(t)] ?? 1,
  }));

  const setVolume = useCallback(
    (key: string, v: number) => {
      setVolumes((old) => ({ ...old, [key]: v }));
      const t = tracks?.find((x) => trackKey(x) === key);
      if (t) saveVolume(t.kind, v);
    },
    [tracks],
  );

  const duration = mediaDuration ?? clip?.duration_s ?? 0;
  const tin = clip?.trim_in ?? 0;
  const tout = Math.min(clip?.trim_out ?? duration, duration || Infinity);
  const hasTrim = clip ? clip.trim_in !== null || clip.trim_out !== null : false;

  // Keep playback inside the trim range; poll per frame for a smooth playhead.
  const range = useRef({ tin, tout });
  range.current = { tin, tout };
  useEffect(() => {
    if (!playing) return;
    let raf = 0;
    const tick = () => {
      const v = videoRef.current;
      if (!v) return;
      const { tout } = range.current;
      if (v.currentTime >= tout - 0.01) {
        v.pause();
        v.currentTime = tout;
      }
      setCurrent(v.currentTime);
      if (separate.current && !v.paused) {
        for (const a of audioEls.current.values()) {
          if (Math.abs(a.currentTime - v.currentTime) > MAX_DRIFT) a.currentTime = v.currentTime;
        }
      }
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [playing]);

  const seek = useCallback((t: number) => {
    const v = videoRef.current;
    if (!v) return;
    const { tin, tout } = range.current;
    const clamped = Math.min(tout, Math.max(tin, t));
    v.currentTime = clamped;
    setCurrent(clamped);
    for (const a of audioEls.current.values()) a.currentTime = clamped;
  }, []);

  /** Pauses and moves one frame back or forward. */
  const stepFrame = useCallback(
    (dir: 1 | -1) => {
      const v = videoRef.current;
      if (!v) return;
      if (!v.paused) v.pause();
      seek(frameStep(v.currentTime, clip?.fps ?? 60, dir));
    },
    [seek, clip?.fps],
  );

  const jumpMarker = useCallback(
    (dir: 1 | -1) => {
      const v = videoRef.current;
      if (!v || !clip) return;
      // Measured from 2 s after the playhead's lead-in point, so a repeat press moves on.
      const t = markerJump(clip.markers, v.currentTime + 2, dir);
      // A 2 s lead-in so the moment itself is seen.
      if (t !== null) seek(Math.max(0, t - 2));
    },
    [seek, clip],
  );

  const toggleStar = async () => {
    if (!clip) return;
    try {
      setClip(await api.setStarred(clip.id, !clip.starred));
    } catch (e) {
      reportError("Could not change the star", e);
    }
  };

  const doShare = async () => {
    if (!clip) return;
    const preset = EXPORT_PRESETS.find((p) => p.id === presetId) ?? EXPORT_PRESETS[0];
    // Without the track list (failed to read), keep the first track as it is.
    const list =
      tracks && !tracksError
        ? exportTracks(tracks.map((t) => ({ index: t.index, key: trackKey(t) })), volumes, muted)
        : [{ index: 0, volume: 1 }];
    setSharing(true);
    setShared(null);
    try {
      setShared(await api.exportClip(clip.id, exportOptions(preset, list)));
    } catch (e) {
      reportError("Export failed", e);
    } finally {
      setSharing(false);
    }
  };

  const toggleFullscreen = useCallback(() => {
    if (document.fullscreenElement) void document.exitFullscreen();
    else void playerRef.current?.requestFullscreen().catch(() => {});
  }, []);

  const togglePlay = useCallback(() => {
    const v = videoRef.current;
    if (!v) return;
    if (v.paused) {
      const { tin, tout } = range.current;
      if (v.currentTime < tin - 0.05 || v.currentTime >= tout - 0.05) v.currentTime = tin;
      v.play().catch((e) => reportError("Could not play the clip", e));
    } else v.pause();
  }, []);

  const clipId = clip?.id;
  const saveMeta = useCallback(
    (next: Partial<Meta>) => {
      const base = wanted.current;
      if (!clipId || !base) return;
      const want = { ...base, ...next };
      if (sameMeta(want, base)) return;
      wanted.current = want;
      if (next.tags) setTags(next.tags);
      const run = saveChain.current.then(async () => {
        try {
          const c = await api.updateClipMeta(clipId, want.title, want.description, want.tags);
          saved.current = metaOf(c);
          if (next.title !== undefined) setTitleError(null);
          // A newer edit is queued: its response updates the view.
          if (wanted.current !== want) return;
          wanted.current = saved.current;
          setClip(c);
          // Show the stored (normalised) text, unless the user typed on meanwhile.
          setTitle((cur) => (cur === want.title ? c.title : cur));
          setDescription((cur) => (cur === want.description ? c.description : cur));
          setTags(c.tags);
        } catch (e) {
          if (wanted.current === want && saved.current) {
            wanted.current = saved.current;
            setTags(saved.current.tags);
          }
          const msg = e instanceof Error ? e.message : String(e);
          if (next.title !== undefined) setTitleError(msg);
          else reportError("Could not save changes", e);
        }
      });
      saveChain.current = run;
    },
    [clipId],
  );

  const changeTrim = useCallback(
    async (a: number | null, b: number | null) => {
      if (!clip) return;
      const nin = a !== null && a > EDGE ? a : null;
      // The backend validates against the stored duration, which can differ a
      // little from the file's; anything at or past the end means "to the end".
      const end = Math.min(duration, clip.duration_s);
      const nout = b !== null && b < end - EDGE ? b : null;
      try {
        const updated = await api.setTrim(clip.id, nin, nout);
        setClip(updated);
        const v = videoRef.current;
        if (v) {
          // Show the frame at the handle that moved.
          const target = (a ?? 0) !== tin ? updated.trim_in ?? 0 : (updated.trim_out ?? duration) - 0.05;
          v.currentTime = Math.max(0, target);
          setCurrent(v.currentTime);
        }
      } catch (e) {
        reportError("Could not set the trim", e);
      }
    },
    [clip, duration, tin],
  );

  const doExport = async () => {
    if (!clip) return;
    setExporting(true);
    try {
      const path = await api.exportTrimmed(clip.id, accurate);
      toast.info(`Exported to ${path}`);
    } catch (e) {
      reportError("Export failed", e);
    } finally {
      setExporting(false);
    }
  };

  const doDelete = async () => {
    if (!clip) return;
    setDeleting(true);
    try {
      await api.deleteClip(clip.id);
      forgetThumb(clip.id);
      toast.info(clip.source === "moments" ? `“${clip.title}” removed from the library` : `“${clip.title}” moved to the Recycle Bin`);
      navigate({ page: "library" });
    } catch (e) {
      reportError("Could not delete the clip", e);
      setDeleting(false);
      setConfirmDelete(false);
    }
  };

  // Keyboard: Space play/pause, ←/→ 5 s, Shift+←/→ markers, ,/. frames,
  // [/] speed, F fullscreen, I/O trim, Esc back.
  useEffect(() => {
    const k = (e: KeyboardEvent) => {
      if (confirmDelete || e.ctrlKey || e.altKey || e.metaKey) return;
      if (isTyping(e.target)) {
        if (e.key === "Escape") (e.target as HTMLElement).blur();
        return;
      }
      const v = videoRef.current;
      switch (e.key) {
        case " ":
          e.preventDefault();
          togglePlay();
          break;
        case "ArrowLeft":
          e.preventDefault();
          if (e.shiftKey) jumpMarker(-1);
          else if (v) seek(v.currentTime - 5);
          break;
        case "ArrowRight":
          e.preventDefault();
          if (e.shiftKey) jumpMarker(1);
          else if (v) seek(v.currentTime + 5);
          break;
        case ",":
          e.preventDefault();
          stepFrame(-1);
          break;
        case ".":
          e.preventDefault();
          stepFrame(1);
          break;
        case "[":
          setRate((r) => stepSpeed(r, -1));
          break;
        case "]":
          setRate((r) => stepSpeed(r, 1));
          break;
        case "f":
        case "F":
          toggleFullscreen();
          break;
        case "i":
        case "I":
          if (v && v.currentTime < tout - 0.5) void changeTrim(v.currentTime, clip?.trim_out ?? null);
          break;
        case "o":
        case "O":
          if (v && v.currentTime > tin + 0.5) void changeTrim(clip?.trim_in ?? null, v.currentTime);
          break;
        case "Escape":
          navigate({ page: "library" });
          break;
      }
    };
    window.addEventListener("keydown", k);
    return () => window.removeEventListener("keydown", k);
  }, [confirmDelete, togglePlay, seek, stepFrame, jumpMarker, toggleFullscreen, changeTrim, clip, tin, tout]);

  if (!clip) {
    return (
      <div className="clipview clipview-empty">
        <a className="back" href="#/">
          ← Library
        </a>
        {loadError ? (
          <div className="empty">
            <p>Could not open this clip.</p>
            <p className="muted">{loadError}</p>
          </div>
        ) : (
          <div className="empty muted">Loading…</div>
        )}
      </div>
    );
  }

  return (
    <div className="clipview">
      <div className="clip-main">
        <div className="clip-head">
          <a className="back" href="#/" title="Back to library (Esc)">
            <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden>
              <path d="M10 3 5 8l5 5" stroke="currentColor" strokeWidth="1.8" fill="none" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
            Library
          </a>
          <input
            className={`title-input${titleError ? " has-error" : ""}`}
            value={title}
            aria-label="Title"
            aria-invalid={Boolean(titleError)}
            onChange={(e) => setTitle(e.target.value)}
            onBlur={() => {
              if (titleCancelled.current) {
                titleCancelled.current = false;
                return;
              }
              saveMeta({ title });
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") e.currentTarget.blur();
              if (e.key === "Escape") {
                titleCancelled.current = true;
                setTitle(wanted.current?.title ?? clip.title);
                setTitleError(null);
              }
            }}
          />
          <button
            type="button"
            className={`btn btn-icon btn-ghost star-btn${clip.starred ? " on" : ""}`}
            aria-pressed={clip.starred}
            aria-label={clip.starred ? "Unstar" : "Star"}
            title={clip.starred ? "Unstar" : "Star"}
            onClick={() => void toggleStar()}
          >
            <StarIcon on={clip.starred} size={18} />
          </button>
        </div>
        {titleError && (
          <p className="field-error" role="alert">
            {titleError}
          </p>
        )}

        <div className="player" ref={playerRef} onClick={togglePlay} onDoubleClick={toggleFullscreen}>
          <video
            key={videoVersion}
            ref={videoRef}
            // The version only makes the URL new, so the swapped file is not served from cache.
            src={fileSrc(clip.path) + (videoVersion ? `?v=${videoVersion}` : "")}
            preload="auto"
            muted={separateAudio}
            onLoadedMetadata={(e) => {
              const v = e.currentTarget;
              if (Number.isFinite(v.duration)) setMediaDuration(v.duration);
              const back = resume.current;
              resume.current = null;
              if (back) v.currentTime = back.t;
              else if (clip.trim_in) v.currentTime = clip.trim_in;
              v.playbackRate = rate;
              setCurrent(v.currentTime);
              if (back?.play) v.play().catch(() => {});
            }}
            onPlay={() => {
              setPlaying(true);
              syncAudio(true);
            }}
            onPause={() => {
              setPlaying(false);
              syncAudio(false);
            }}
            onWaiting={() => syncAudio(false)}
            onPlaying={() => syncAudio(true)}
            onSeeked={(e) => {
              setCurrent(e.currentTarget.currentTime);
              syncAudio(!e.currentTarget.paused);
            }}
            onError={() => setVideoError(true)}
          />
          {tracks?.map((t) => (
            <audio
              key={trackKey(t)}
              src={fileSrc(t.path)}
              preload="auto"
              ref={(el) => {
                if (el) {
                  el.volume = volumes[trackKey(t)] ?? 1;
                  el.playbackRate = rate;
                  audioEls.current.set(trackKey(t), el);
                } else audioEls.current.delete(trackKey(t));
              }}
              onError={() => setAudioFailed(true)}
            />
          ))}
          {compressStage === "swapping" && !videoError && (
            <div className="player-error">
              <p>Switching to the compressed file…</p>
            </div>
          )}
          {videoError && (
            <div className="player-error">
              <p>This video file could not be played.</p>
              <p className="muted">{clip.path}</p>
            </div>
          )}
        </div>

        <div className="transport">
          <button type="button" className="btn btn-icon play" onClick={togglePlay} aria-label={playing ? "Pause (Space)" : "Play (Space)"}>
            {playing ? (
              <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden>
                <rect x="3.5" y="2.5" width="3" height="11" rx="0.6" fill="currentColor" />
                <rect x="9.5" y="2.5" width="3" height="11" rx="0.6" fill="currentColor" />
              </svg>
            ) : (
              <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden>
                <path d="M4 2.5v11l9.5-5.5Z" fill="currentColor" />
              </svg>
            )}
          </button>
          <span className="step-group" role="group" aria-label="Step">
            <button type="button" className="btn btn-icon btn-small btn-ghost" onClick={() => jumpMarker(-1)} title="Previous marker (Shift+←)" aria-label="Previous marker">
              <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden>
                <path d="M3.5 3v10M12.5 3.5 6 8l6.5 4.5Z" stroke="currentColor" strokeWidth="1.5" fill="currentColor" strokeLinejoin="round" />
              </svg>
            </button>
            <button type="button" className="btn btn-icon btn-small btn-ghost" onClick={() => stepFrame(-1)} title="Previous frame (,)" aria-label="Previous frame">
              <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden>
                <path d="M10.5 3.5 6 8l4.5 4.5" stroke="currentColor" strokeWidth="1.8" fill="none" strokeLinecap="round" strokeLinejoin="round" />
              </svg>
            </button>
            <button type="button" className="btn btn-icon btn-small btn-ghost" onClick={() => stepFrame(1)} title="Next frame (.)" aria-label="Next frame">
              <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden>
                <path d="M5.5 3.5 10 8l-4.5 4.5" stroke="currentColor" strokeWidth="1.8" fill="none" strokeLinecap="round" strokeLinejoin="round" />
              </svg>
            </button>
            <button type="button" className="btn btn-icon btn-small btn-ghost" onClick={() => jumpMarker(1)} title="Next marker (Shift+→)" aria-label="Next marker">
              <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden>
                <path d="M12.5 3v10M3.5 3.5 10 8l-6.5 4.5Z" stroke="currentColor" strokeWidth="1.5" fill="currentColor" strokeLinejoin="round" />
              </svg>
            </button>
          </span>
          <select className="speed" value={rate} onChange={(e) => setRate(Number(e.target.value))} aria-label="Playback speed" title="Playback speed ([ and ])">
            {SPEEDS.map((s) => (
              <option key={s} value={s}>
                {formatSpeed(s)}
              </option>
            ))}
          </select>
          <span className="clock">
            {formatClock(current)} <span className="muted">/ {formatDuration(duration)}</span>
          </span>
          <span className="trim-info">
            {hasTrim ? (
              <>
                Trim {formatClock(tin)} – {formatClock(tout)} <span className="muted">({formatDuration(tout - tin)})</span>
              </>
            ) : (
              <span className="muted">Drag the handles or press I / O to trim</span>
            )}
          </span>
          <span className="transport-actions">
            <button type="button" className="btn btn-small" onClick={() => void changeTrim(current, clip.trim_out)} title="Set trim start at playhead (I)">
              Start here
            </button>
            <button type="button" className="btn btn-small" onClick={() => void changeTrim(clip.trim_in, current)} title="Set trim end at playhead (O)">
              End here
            </button>
            <button type="button" className="btn btn-small btn-ghost" disabled={!hasTrim} onClick={() => void changeTrim(null, null)}>
              Clear trim
            </button>
          </span>
        </div>

        <Timeline
          duration={duration}
          markers={clip.markers}
          trimIn={clip.trim_in ?? undefined}
          trimOut={clip.trim_out ?? undefined}
          current={current}
          clipLabel={clip.source === "moments" ? "Moments clip" : "Clip"}
          tracks={timelineTracks}
          tracksError={tracksError}
          onVolume={separateAudio ? setVolume : undefined}
          onSeek={seek}
          onTrimChange={(a, b) => void changeTrim(a, b)}
        />
        <p className="keys muted">
          <kbd>Space</kbd> play/pause <kbd>←</kbd>
          <kbd>→</kbd> 5 s <kbd>Shift</kbd>+<kbd>←</kbd>
          <kbd>→</kbd> markers <kbd>,</kbd>
          <kbd>.</kbd> frame <kbd>[</kbd>
          <kbd>]</kbd> speed <kbd>F</kbd> fullscreen <kbd>I</kbd>
          <kbd>O</kbd> trim <kbd>Esc</kbd> back
        </p>
      </div>

      <aside className="clip-side">
        <dl className="facts">
          <div>
            <dt>Game</dt>
            <dd>{clip.game || "Unknown"}</dd>
          </div>
          <div>
            <dt>Taken</dt>
            <dd>{formatTakenAt(clip.recorded_at)}</dd>
          </div>
          <div>
            <dt>Video</dt>
            <dd>
              {clip.height}p{clip.fps ? `${clip.fps}` : ""}, {formatBytes(clip.size)}
            </dd>
          </div>
          <div>
            <dt>Source</dt>
            <dd>{clip.source === "moments" ? "Imported from Moments" : "ClipVault"}</dd>
          </div>
        </dl>

        <label className="field">
          <span className="field-label">Description</span>
          <textarea
            rows={3}
            value={description}
            placeholder="What happened?"
            onChange={(e) => setDescription(e.target.value)}
            onBlur={() => saveMeta({ description })}
          />
        </label>

        <div className="field">
          <span className="field-label">Tags</span>
          <TagEditor tags={tags} onChange={(t) => saveMeta({ tags: t })} />
        </div>

        <div className="side-block">
          <h3>Share</h3>
          <label className="field">
            <span className="field-label">Preset</span>
            <select value={presetId} onChange={(e) => setPresetId(e.target.value)}>
              {EXPORT_PRESETS.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
          </label>
          <p className="hint">{EXPORT_PRESETS.find((p) => p.id === presetId)?.hint}</p>
          <div className="field">
            <span className="field-label">Audio</span>
            {tracks && !tracksError ? (
              <ul className="export-tracks">
                {tracks.map((t) => {
                  const k = trackKey(t);
                  const vol = volumes[k] ?? 1;
                  return (
                    <li key={k}>
                      <label className="check">
                        <input type="checkbox" checked={!muted[k]} onChange={(e) => setMuted((m) => ({ ...m, [k]: !e.target.checked }))} />
                        <span>{t.label}</span>
                      </label>
                      <span className="muted">{muted[k] ? "off" : `${Math.round(vol * 100)}%`}</span>
                    </li>
                  );
                })}
              </ul>
            ) : (
              <p className="hint">{tracksError ? "Tracks could not be read; the first audio track is kept as is." : "Reading tracks…"}</p>
            )}
            {tracks && !tracksError && <p className="hint">Mixed into one track. Volumes come from the sliders under the timeline.</p>}
          </div>
          <button type="button" className="btn btn-primary btn-wide" disabled={sharing || (!tracks && !tracksError)} onClick={() => void doShare()}>
            {sharing ? "Exporting…" : hasTrim ? "Export trim range" : "Export whole clip"}
          </button>
          {shared && (
            <div className="export-done">
              <code title={shared}>{fileName(shared)}</code>
              <button type="button" className="btn btn-small" onClick={() => api.revealInFolder(shared).catch((e) => reportError("Could not open the folder", e))}>
                Show in folder
              </button>
            </div>
          )}
        </div>

        <div className="side-block">
          <h3>Save trim as clip</h3>
          <label className="check">
            <input type="checkbox" checked={accurate} onChange={(e) => setAccurate(e.target.checked)} />
            <span>
              Frame-accurate
              <small>Re-encodes the cut. Slower; without it the cut snaps to the nearest keyframe.</small>
            </span>
          </label>
          <button type="button" className="btn btn-wide" disabled={!hasTrim || exporting} onClick={() => void doExport()}>
            {exporting ? "Exporting…" : "Export trimmed clip"}
          </button>
          {!hasTrim && <p className="hint">Set a trim range first.</p>}
          {hasTrim && (
            <p className="hint">
              {clip.source === "moments"
                ? "Saved as a new clip in your ClipVault folder (the Moments folder is left alone)."
                : "Saved as a new clip next to the original."}
            </p>
          )}
        </div>

        {!clip.compressed && (
          <div className="side-block">
            <button
              type="button"
              className="btn btn-wide"
              disabled={compressStage !== null}
              title="Re-encodes the video to AV1 in the background; audio stays as it is"
              onClick={() => {
                setCompressStage("asked");
                api.compressClip(clip.id).then(
                  () => toast.info("Compressing in the background. The player reloads briefly when it is done."),
                  (e) => {
                    setCompressStage((s) => (s === "asked" ? null : s));
                    reportError("Could not start compression", e);
                  },
                );
              }}
            >
              {compressStage === null
                ? "Compress to save space"
                : compressStage === "swapping"
                  ? "Switching file…"
                  : compressStage === "encoding"
                    ? "Compressing…"
                    : "Waiting to compress…"}
            </button>
          </div>
        )}

        <div className="side-block side-path">
          <span className="field-label">File</span>
          <code title={clip.path}>{clip.path}</code>
        </div>

        <button type="button" className="btn btn-danger-ghost btn-wide" onClick={() => setConfirmDelete(true)}>
          {clip.source === "moments" ? "Remove from library" : "Delete clip"}
        </button>
      </aside>

      <ConfirmDialog
        open={confirmDelete}
        title={clip.source === "moments" ? "Remove this clip from the library?" : "Delete this clip?"}
        confirmLabel={deleting ? "Removing…" : clip.source === "moments" ? "Remove" : "Move to Recycle Bin"}
        danger
        busy={deleting}
        onConfirm={() => void doDelete()}
        onCancel={() => setConfirmDelete(false)}
      >
        {clip.source === "moments" ? (
          <p>
            “{clip.title}” is removed from ClipVault and will not come back on a later import. The file belongs to
            SteelSeries and stays where it is: {clip.path}
          </p>
        ) : (
          <p>The file of “{clip.title}” moves to the Recycle Bin and the clip is removed from your library.</p>
        )}
      </ConfirmDialog>
    </div>
  );
}
