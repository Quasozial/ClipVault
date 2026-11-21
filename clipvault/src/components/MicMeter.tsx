import { useEffect, useRef, useState } from "react";
import { on } from "../api";

export const METER_MIN_DB = -60;
const pct = (db: number) => `${Math.min(100, Math.max(0, ((db - METER_MIN_DB) / -METER_MIN_DB) * 100))}%`;

/**
 * Live mic level (`mic-level` events) with the laugh threshold slider laid over
 * it: the part of the bar above the threshold lights up, so you can see what
 * would count as a laugh. Peak hold decays slowly.
 */
export function MicMeter(props: { threshold: number; onThreshold(v: number): void; disabled?: boolean }) {
  const [level, setLevel] = useState<number | null>(null);
  const [peak, setPeak] = useState(METER_MIN_DB);
  const peakAt = useRef(0);

  useEffect(
    () =>
      on("mic-level", ({ dbfs }) => {
        const v = Number.isFinite(dbfs) ? dbfs : METER_MIN_DB;
        setLevel(v);
        const now = performance.now();
        setPeak((p) => {
          if (v >= p || now - peakAt.current > 1200) {
            peakAt.current = now;
            return v;
          }
          return p;
        });
      }),
    [],
  );

  const lvl = level ?? METER_MIN_DB;
  const over = lvl > props.threshold;
  return (
    <div className={`meter${props.disabled ? " is-disabled" : ""}`}>
      <div className="meter-bar" aria-hidden>
        <div className="meter-fill" style={{ width: pct(Math.min(lvl, props.threshold)) }} />
        {over && <div className="meter-over" style={{ left: pct(props.threshold), width: `calc(${pct(lvl)} - ${pct(props.threshold)})` }} />}
        <div className="meter-peak" style={{ left: pct(peak) }} />
        <div className="meter-threshold" style={{ left: pct(props.threshold) }} />
      </div>
      <input
        className="meter-slider"
        type="range"
        min={METER_MIN_DB}
        max={0}
        step={1}
        value={props.threshold}
        disabled={props.disabled}
        aria-label="Laugh threshold (dBFS)"
        aria-valuetext={`${props.threshold} dB`}
        onChange={(e) => props.onThreshold(Number(e.target.value))}
      />
      <div className="meter-scale" aria-hidden>
        <span>-60 dB</span>
        <span>-40</span>
        <span>-20</span>
        <span>0</span>
      </div>
      <div className="meter-read">
        <span>
          Threshold <b>{props.threshold} dB</b>
        </span>
        <span className="muted">{level === null ? "Waiting for the microphone…" : `Now ${Math.round(lvl)} dB`}</span>
      </div>
    </div>
  );
}
