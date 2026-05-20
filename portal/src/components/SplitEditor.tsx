import type { ReactNode } from "react";

import { YamlPreview } from "./YamlPreview";

/**
 * Two-column layout used by every "form + live YAML preview" page.
 * Caller owns form state and passes the assembled CRD object to `value`.
 */
export function SplitEditor({
  form,
  value,
  filename,
}: {
  form: ReactNode;
  value: unknown;
  filename: string;
}) {
  return (
    <div className="grid grid-cols-2 gap-3 h-[calc(100vh-9rem)]">
      <div className="card overflow-auto p-4">{form}</div>
      <YamlPreview value={value} filename={filename} />
    </div>
  );
}
