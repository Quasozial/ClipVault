import { useEffect, useRef, type ReactNode } from "react";
import type { Health } from "../api";
import { dismissToast, useToasts } from "../lib/toast";

export function Toasts() {
  const list = useToasts();
  return (
    <div className="toasts" role="status" aria-live="polite">
      {list.map((t) => (
        <div key={t.id} className={`toast toast-${t.kind}`} role={t.kind === "error" ? "alert" : undefined}>
          <span>{t.text}</span>
          <button type="button" className="icon-btn" aria-label="Dismiss" onClick={() => dismissToast(t.id)}>
            <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden>
              <path d="M2 2l8 8M10 2l-8 8" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
            </svg>
          </button>
        </div>
      ))}
    </div>
  );
}

/** Modal confirm built on <dialog>: focus is trapped, Esc cancels. */
export function ConfirmDialog(props: {
  open: boolean;
  title: string;
  children: ReactNode;
  confirmLabel: string;
  danger?: boolean;
  busy?: boolean;
  onConfirm(): void;
  onCancel(): void;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (props.open && !d.open) d.showModal();
    if (!props.open && d.open) d.close();
  }, [props.open]);
  return (
    <dialog
      ref={ref}
      className="dialog"
      onCancel={(e) => {
        e.preventDefault();
        if (!props.busy) props.onCancel();
      }}
      onKeyDown={(e) => e.stopPropagation()}
    >
      <h2>{props.title}</h2>
      <div className="dialog-body">{props.children}</div>
      <div className="dialog-actions">
        <button type="button" className="btn" onClick={props.onCancel} disabled={props.busy} autoFocus>
          Cancel
        </button>
        <button
          type="button"
          className={`btn ${props.danger ? "btn-danger" : "btn-primary"}`}
          onClick={props.onConfirm}
          disabled={props.busy}
        >
          {props.confirmLabel}
        </button>
      </div>
    </dialog>
  );
}

export function healthText(h: Health | null): { tone: "idle" | "rec" | "warn" | "error"; text: string } {
  if (!h) return { tone: "idle", text: "Connecting…" };
  switch (h.state) {
    case "recording":
      return { tone: "rec", text: h.target ? `Recording – ${h.target}` : "Recording" };
    case "restarting":
      return { tone: "warn", text: `Restarting – ${h.reason ?? "capture"}` };
    case "failing":
      return { tone: "error", text: `Error – ${h.reason ?? "unknown"}` };
    default:
      return { tone: "idle", text: "Idle" };
  }
}

export function HealthStatus({ health }: { health: Health | null }) {
  const { tone, text } = healthText(health);
  return (
    <div className={`health health-${tone}`} title={text} aria-live="polite">
      <span className="health-dot" aria-hidden />
      <span className="health-text">{text}</span>
    </div>
  );
}

export function Toggle(props: {
  checked: boolean;
  onChange(v: boolean): void;
  label: ReactNode;
  hint?: ReactNode;
  disabled?: boolean;
}) {
  return (
    <label className={`toggle${props.disabled ? " is-disabled" : ""}`}>
      <input
        type="checkbox"
        role="switch"
        checked={props.checked}
        disabled={props.disabled}
        onChange={(e) => props.onChange(e.target.checked)}
      />
      <span className="toggle-track" aria-hidden>
        <span className="toggle-knob" />
      </span>
      <span className="toggle-text">
        <span>{props.label}</span>
        {props.hint && <small>{props.hint}</small>}
      </span>
    </label>
  );
}
