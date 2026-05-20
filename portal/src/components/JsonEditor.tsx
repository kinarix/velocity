import { useEffect, useState } from "react";

/**
 * Plain <textarea> JSON editor with parse-error feedback. Avoids pulling in
 * a heavyweight code editor — sufficient for record edit in MVP.
 */
export function JsonEditor({
  value,
  onChange,
  rows = 20,
}: {
  value: unknown;
  onChange: (parsed: unknown) => void;
  rows?: number;
}) {
  const [text, setText] = useState(() => JSON.stringify(value, null, 2));
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    setText(JSON.stringify(value, null, 2));
  }, [value]);

  return (
    <div>
      <textarea
        rows={rows}
        spellCheck={false}
        className="input font-mono text-xs leading-5"
        value={text}
        onChange={(e) => {
          const t = e.target.value;
          setText(t);
          try {
            const parsed = JSON.parse(t);
            setErr(null);
            onChange(parsed);
          } catch (ex) {
            setErr((ex as Error).message);
          }
        }}
      />
      {err && <div className="text-[11px] text-red-300 mt-1">JSON parse: {err}</div>}
    </div>
  );
}
