import { useState } from "react";

import { PageTitle } from "../components/PageTitle";
import { SplitEditor } from "../components/SplitEditor";
import { applyObject, type KubeObject } from "../api/admin";

export function LogFilterEditor() {
  const [name, setName] = useState("redact-pii");
  const [namespace, setNamespace] = useState("acme-supply-chain-procurement");
  const [rulesText, setRulesText] = useState(
    `- match:
    field: "sensitivity"
    op: "in"
    values: ["pii", "financial"]
  action: "redact"
- match:
    field: "level"
    op: "lt"
    value: "info"
  action: "drop"`,
  );

  const value = {
    apiVersion: "velocity.sh/v1",
    kind: "LogFilterPolicy",
    metadata: { name, namespace },
    spec: {
      // Body is intentionally passed as-is; the LogProcessor parses it.
      // The visual editor surfaces it as YAML so users can write the
      // rules naturally without us imposing a half-baked schema.
      rulesYaml: rulesText,
    },
  };

  return (
    <>
      <PageTitle
        title="New LogFilterPolicy"
        subtitle="Match + action rules applied by velocity-log-processor before storage."
      />
      <SplitEditor
        filename={`${name}.logfilter.yaml`}
        value={value}
        onApply={() => applyObject("LogFilterPolicy", namespace, name, value as KubeObject)}
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
              <label className="label">Rules (YAML — paste under spec.rules)</label>
              <textarea
                rows={16}
                className="input font-mono"
                value={rulesText}
                onChange={(e) => setRulesText(e.target.value)}
              />
            </div>
          </div>
        }
      />
    </>
  );
}
