export function StarIcon({ on, size = 16 }: { on: boolean; size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" aria-hidden>
      <path
        d="M8 1.6l1.95 4.1 4.45.55-3.27 3.08.84 4.42L8 11.6l-3.97 2.15.84-4.42L1.6 6.25l4.45-.55z"
        fill={on ? "currentColor" : "none"}
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinejoin="round"
      />
    </svg>
  );
}
