import { useState } from "react";

import { PageTitle } from "../components/PageTitle";
import { SplitEditor } from "../components/SplitEditor";

export function RoleBindingEditor() {
  const [name, setName] = useState("ravi-procurement-reader");
  const [namespace, setNamespace] = useState("acme-supply-chain-procurement");
  const [role, setRole] = useState("procurement-reader");
  const [subjects, setSubjects] = useState("user:ravi.kumar");
  const [scopesText, setScopesText] = useState('field: "store_id"\nvalues: ["STR-001", "STR-007"]');

  let scope: unknown = undefined;
  try {
    const lines = scopesText.split("\n").map((l) => l.trim()).filter(Boolean);
    const m: Record<string, unknown> = {};
    for (const line of lines) {
      const ix = line.indexOf(":");
      if (ix < 0) continue;
      const k = line.slice(0, ix).trim();
      const v = line.slice(ix + 1).trim();
      try {
        m[k] = JSON.parse(v);
      } catch {
        m[k] = v.replace(/^"|"$/g, "");
      }
    }
    scope = Object.keys(m).length ? m : undefined;
  } catch {
    /* leave undefined */
  }

  const value = {
    apiVersion: "velocity.sh/v1",
    kind: "RoleBinding",
    metadata: { name, namespace },
    spec: {
      role,
      subjects: subjects.split(",").map((s) => s.trim()).filter(Boolean),
      ...(scope ? { scope } : {}),
    },
  };

  return (
    <>
      <PageTitle
        title="New RoleBinding"
        subtitle="Bind subjects to a role with optional ABAC scope."
      />
      <SplitEditor
        filename={`${name}.rolebinding.yaml`}
        value={value}
        form={
          <div className="space-y-3 text-xs">
            <div>
              <label className="label">Name</label>
              <input className="input" value={name} onChange={(e) => setName(e.target.value)} />
            </div>
            <div>
              <label className="label">Namespace</label>
              <input className="input" value={namespace} onChange={(e) => setNamespace(e.target.value)} />
            </div>
            <div>
              <label className="label">Role</label>
              <input className="input" value={role} onChange={(e) => setRole(e.target.value)} />
            </div>
            <div>
              <label className="label">Subjects (comma-separated)</label>
              <input className="input" value={subjects} onChange={(e) => setSubjects(e.target.value)} />
            </div>
            <div>
              <label className="label">Scope (YAML key: value pairs)</label>
              <textarea
                rows={6}
                className="input font-mono"
                value={scopesText}
                onChange={(e) => setScopesText(e.target.value)}
              />
              <div className="text-[11px] text-ink-400 mt-1">
                Optional. Maps to <code>spec.scope</code> for ABAC filtering.
              </div>
            </div>
          </div>
        }
      />
    </>
  );
}
