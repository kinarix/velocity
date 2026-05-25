import { useMemo, useState } from "react";
import YAML from "yaml";

import { copy } from "../util/clipboard";

export function YamlPreview({
  value,
  filename,
  onApply,
}: {
  value: unknown;
  filename: string;
  onApply?: () => Promise<unknown>;
}) {
  const yaml = useMemo(() => {
    try {
      return YAML.stringify(value, { sortMapEntries: false });
    } catch (e) {
      return `# YAML serialization failed: ${(e as Error).message}\n`;
    }
  }, [value]);

  const [copied, setCopied] = useState<"yaml" | "cmd" | null>(null);
  const [applyState, setApplyState] = useState<"idle" | "pending" | "ok" | "error">("idle");
  const [applyError, setApplyError] = useState("");

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

  const handleApply = async () => {
    if (!onApply || applyState === "pending") return;
    setApplyState("pending");
    setApplyError("");
    try {
      await onApply();
      setApplyState("ok");
      setTimeout(() => setApplyState("idle"), 2000);
    } catch (e) {
      setApplyError((e as Error).message ?? "unknown error");
      setApplyState("error");
      setTimeout(() => setApplyState("idle"), 4000);
    }
  };

  return (
    <div className="card flex flex-col h-full">
      <div className="panel-title flex justify-between items-center">
        <span>{filename}</span>
        <div className="flex gap-1 items-center">
          <button className="btn btn-ghost" onClick={() => void onCopy("yaml")}>
            {copied === "yaml" ? "✓ copied" : "Copy YAML"}
          </button>
          <button className="btn btn-ghost" onClick={() => void onCopy("cmd")}>
            {copied === "cmd" ? "✓ copied" : "Copy command"}
          </button>
          {onApply && (
            <button
              className={[
                "btn",
                applyState === "ok" ? "btn-ghost text-green-400" :
                applyState === "error" ? "btn-ghost text-red-400" :
                "btn-primary",
              ].join(" ")}
              disabled={applyState === "pending"}
              onClick={() => void handleApply()}
            >
              {applyState === "pending" ? "Applying…" :
               applyState === "ok" ? "✓ Applied" :
               applyState === "error" ? `✗ ${applyError}` :
               "Apply to cluster"}
            </button>
          )}
        </div>
      </div>
      <pre className="text-xs leading-5 p-3 overflow-auto flex-1">
        <code>{yaml}</code>
      </pre>
    </div>
  );
}
