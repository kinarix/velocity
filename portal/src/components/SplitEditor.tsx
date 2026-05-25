import type { ReactNode } from "react";

import { YamlPreview } from "./YamlPreview";

export function SplitEditor({
  form,
  value,
  filename,
  onApply,
}: {
  form: ReactNode;
  value: unknown;
  filename: string;
  onApply?: () => Promise<unknown>;
}) {
  return (
    <div className="grid grid-cols-2 gap-3 h-[calc(100vh-9rem)]">
      <div className="card overflow-auto p-4">{form}</div>
      <YamlPreview value={value} filename={filename} onApply={onApply} />
    </div>
  );
}
