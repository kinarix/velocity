import { lazy, Suspense } from "react";

// Lazy-load Monaco to avoid blocking the initial bundle.
const MonacoEditor = lazy(() => import("@monaco-editor/react"));

interface Props {
  value: string;
  onChange?: (value: string) => void;
  readOnly?: boolean;
  height?: string;
}

export function YamlEditor({ value, onChange, readOnly = false, height = "100%" }: Props) {
  return (
    <Suspense
      fallback={
        <textarea
          className="w-full h-full font-mono text-xs bg-gray-900 text-gray-100 p-3 resize-none outline-none"
          value={value}
          readOnly={readOnly}
          onChange={(e) => onChange?.(e.target.value)}
          style={{ height }}
        />
      }
    >
      <MonacoEditor
        height={height}
        language="yaml"
        theme="vs-dark"
        value={value}
        options={{
          readOnly,
          minimap: { enabled: false },
          fontSize: 12,
          lineNumbers: "on",
          wordWrap: "on",
          scrollBeyondLastLine: false,
          tabSize: 2,
        }}
        onChange={(v) => !readOnly && onChange?.(v ?? "")}
      />
    </Suspense>
  );
}
