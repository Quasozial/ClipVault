import { useState } from "react";
import { addTags } from "../lib/tags";
import { StarIcon } from "./StarIcon";

/** Replaces the filter chips while clips are selected. */
export function BulkBar(props: {
  count: number;
  sizeText: string;
  busy: boolean;
  onStar(starred: boolean): void;
  onAddTags(tags: string[]): void;
  onCompress(): void;
  onExport(): void;
  onDelete(): void;
  onCancel(): void;
}) {
  const [tagging, setTagging] = useState(false);
  const [input, setInput] = useState("");

  const commit = () => {
    const { tags } = addTags([], input, true);
    if (tags.length) props.onAddTags(tags);
    setInput("");
    setTagging(false);
  };

  return (
    <div className="bulkbar" role="toolbar" aria-label="Selected clips">
      <strong className="bulk-count" aria-live="polite">
        {props.count} selected <span>· {props.sizeText}</span>
      </strong>
      <button type="button" className="btn" disabled={props.busy} onClick={() => props.onStar(true)}>
        <StarIcon on size={14} /> Star
      </button>
      <button type="button" className="btn" disabled={props.busy} onClick={() => props.onStar(false)}>
        <StarIcon on={false} size={14} /> Unstar
      </button>
      {tagging ? (
        <span className="bulk-tag">
          <input
            autoFocus
            value={input}
            placeholder="tag, another"
            aria-label="Tags to add"
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => {
              e.stopPropagation();
              if (e.key === "Enter") commit();
              if (e.key === "Escape") setTagging(false);
            }}
          />
          <button type="button" className="btn btn-primary" onClick={commit} disabled={!input.trim()}>
            Add
          </button>
        </span>
      ) : (
        <button type="button" className="btn" disabled={props.busy} onClick={() => setTagging(true)}>
          Add tag…
        </button>
      )}
      <button type="button" className="btn" disabled={props.busy} onClick={props.onCompress}>
        Compress
      </button>
      <button type="button" className="btn" disabled={props.busy} onClick={props.onExport}>
        Export…
      </button>
      <button type="button" className="btn btn-danger-ghost" disabled={props.busy} onClick={props.onDelete}>
        Delete
      </button>
      <button type="button" className="btn btn-ghost bulk-cancel" onClick={props.onCancel}>
        Cancel
      </button>
    </div>
  );
}
