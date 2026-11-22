/** Library date headings: Today, Yesterday, This week, Last week, then months, then years. Local time, weeks start on Monday. */

function startOfDay(d: Date): Date {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate());
}

function startOfWeek(d: Date): Date {
  const s = startOfDay(d);
  const sinceMonday = (s.getDay() + 6) % 7;
  return new Date(s.getFullYear(), s.getMonth(), s.getDate() - sinceMonday);
}

export function groupLabel(at: Date, now: Date): string {
  const today = startOfDay(now);
  if (at >= today) return "Today";
  const yesterday = new Date(today.getFullYear(), today.getMonth(), today.getDate() - 1);
  if (at >= yesterday) return "Yesterday";
  const week = startOfWeek(now);
  if (at >= week) return "This week";
  if (at >= new Date(week.getFullYear(), week.getMonth(), week.getDate() - 7)) return "Last week";
  if (at.getFullYear() === now.getFullYear()) return `${at.toLocaleString("en", { month: "long" })} ${at.getFullYear()}`;
  return String(at.getFullYear());
}

export interface Group<T> {
  label: string;
  items: T[];
}

export function groupByDate<T>(items: T[], dateOf: (t: T) => Date, now: Date): Group<T>[] {
  const out: Group<T>[] = [];
  for (const item of items) {
    const label = groupLabel(dateOf(item), now);
    const last = out[out.length - 1];
    if (last && last.label === label) last.items.push(item);
    else out.push({ label, items: [item] });
  }
  return out;
}
