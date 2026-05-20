import { useMemo, useState } from "react";
import YAML from "yaml";

import { copy } from "../util/clipboard";

/**
 * Two-pane editor surface: caller controls the form on the left, this component
 * renders the live YAML preview on the right with copy + apply-command actions.
 *
 * "Apply" here means "copy the `kubectl apply` / `velocity apply` command" —
 * the portal doesn't proxy CRD writes to the cluster. Phase 10 of phases.md
 * documents this as the intended workflow.
 */
export function YamlPreview({ value, filename }: { value: unknown; filename: string }) {
  const yaml = useMemo(() => {
    try {
      return YAML.stringify(value, { sortMapEntries: false });
    } catch (e) {
      return `# YAML serialization failed: ${(e as Error).message}\n`;
    }
  }, [value]);

  const [copied, setCopied] = useState<"yaml" | "cmd" | null>(null);

  const onCopy = async (what: "yaml" | "cmd") => {
    const text =
      what === "yaml"
        ? yaml
        : `cat <<'EOF' | velocity apply -f -\n${yaml}EOF`;
    if (await copy(text)) {
      setCopied(what);
      setTimeout(() => setCopied(null), 1500);
    }
  };

  return (
    <div className="card flex flex-col h-full">
      <div className="panel-title flex justify-between items-center">
        <span>{filename}</span>
        <div className="flex gap-1">
          <button className="btn btn-ghost" onClick={() => void onCopy("yaml")}>
            {copied === "yaml" ? "✓ copied" : "Copy YAML"}
          </button>
          <button className="btn btn-ghost" onClick={() => void onCopy("cmd")}>
            {copied === "cmd" ? "✓ copied" : "Copy command"}
          </button>
        </div>
      </div>
      <pre className="text-xs leading-5 p-3 overflow-auto flex-1">
        <code>{yaml}</code>
      </pre>
    </div>
  );
}
