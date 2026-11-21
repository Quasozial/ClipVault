import { useState } from "react";
import { acceleratorFromKey, heldModifiers } from "../lib/hotkey";

/** Click, then press the key combination. Esc cancels; leaving the field cancels. */
export function HotkeyRecorder(props: { value: string; onChange(v: string): void; error?: string; label: string }) {
  const [recording, setRecording] = useState(false);
  const [held, setHeld] = useState("");

  const keys = (recording ? (held ? `${held}+…` : "") : props.value).split("+").filter(Boolean);

  return (
    <div className="hotkey">
      <button
        type="button"
        className={`hotkey-btn${recording ? " is-recording" : ""}${props.error ? " has-error" : ""}`}
        aria-label={`${props.label}: ${props.value}. Click to change`}
        onClick={() => {
          setHeld("");
          setRecording((r) => !r);
        }}
        onBlur={() => setRecording(false)}
        onKeyDown={(e) => {
          if (!recording) return;
          e.preventDefault();
          e.stopPropagation();
          if (e.key === "Escape" && !e.ctrlKey && !e.altKey && !e.shiftKey) {
            setRecording(false);
            return;
          }
          const acc = acceleratorFromKey(e);
          if (acc) {
            props.onChange(acc);
            setRecording(false);
          } else setHeld(heldModifiers(e));
        }}
        onKeyUp={(e) => recording && setHeld(heldModifiers(e))}
      >
        {keys.length > 0 ? (
          keys.map((k, i) => <kbd key={i}>{k}</kbd>)
        ) : (
          <span className="muted">Press keys…</span>
        )}
      </button>
      {recording && <small className="muted">Press the new combination, Esc to cancel</small>}
      {props.error && (
        <small className="field-error" role="alert">
          {props.error}
        </small>
      )}
    </div>
  );
}
