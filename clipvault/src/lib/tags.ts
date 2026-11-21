/**
 * Adds the comma-separated tags typed into the tag field. Everything before the
 * last comma is complete; the text after it stays in the field (`rest`) unless
 * `commitRest` (Enter / leaving the field). Blank and duplicate tags (ignoring
 * case) are skipped.
 */
export function addTags(tags: string[], input: string, commitRest = false): { tags: string[]; rest: string } {
  const parts = input.split(",");
  const rest = commitRest ? "" : parts.pop()!;
  const out = [...tags];
  for (const raw of parts) {
    const t = raw.trim();
    if (t && !out.some((x) => x.toLowerCase() === t.toLowerCase())) out.push(t);
  }
  return { tags: out, rest: commitRest ? "" : rest.trimStart() };
}
