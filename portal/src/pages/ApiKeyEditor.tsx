import { useState } from "react";

import { PageTitle } from "../components/PageTitle";
import { SplitEditor } from "../components/SplitEditor";
import { applyObject, type KubeObject } from "../api/admin";

export function ApiKeyEditor() {
  const [name, setName] = useState("ingest-key");
  const [namespace, setNamespace] = useState("acme-supply-chain-procurement");
  const [scopesText, setScopesText] = useState(
    "schemas:\n  - acme/supply-chain/procurement/purchase-order/v1\noperations:\n  - read\n  - create",
  );
  const [expiresAt, setExpiresAt] = useState("");

  let scopes: unknown;
  try {
    // Naïve YAML-ish parser — relies on the user keeping shape sane. The
    // preview always renders whatever object we assemble, so they see
    // immediately if it broke.
    const obj: Record<string, string[]> = {};
    let current: string | null = null;
    for (const raw of scopesText.split("\n")) {
      const line = raw.replace(/\t/g, "  ");
      if (!line.trim()) continue;
      if (!line.startsWith(" ")) {
        const k = line.replace(":", "").trim();
        obj[k] = [];
        current = k;
      } else if (current && line.trim().startsWith("-")) {
        obj[current].push(line.trim().replace(/^-\s*/, ""));
      }
    }
    scopes = obj;
  } catch {
    scopes = {};
  }

  const value = {
    apiVersion: "velocity.sh/v1",
    kind: "ApiKey",
    metadata: { name, namespace },
    spec: {
      scopes,
      ...(expiresAt ? { expiresAt } : {}),
    },
  };

  return (
    <>
      <PageTitle
        title="New ApiKey"
        subtitle="Plaintext key is shown ONCE by the API server after apply. Store it then."
      />
      <SplitEditor
        filename={`${name}.apikey.yaml`}
        value={value}
        onApply={() => applyObject("ApiKey", namespace, name, value as KubeObject)}
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
              <label className="label">Expires at (RFC 3339, optional)</label>
              <input className="input" value={expiresAt} onChange={(e) => setExpiresAt(e.target.value)} />
            </div>
            <div>
              <label className="label">Scopes (YAML)</label>
              <textarea
                rows={10}
                className="input font-mono"
                value={scopesText}
                onChange={(e) => setScopesText(e.target.value)}
              />
            </div>
            <div className="text-[11px] text-ink-400">
              Only SHA-256 of the key is persisted. The plaintext appears once in the apply
              response — copy it then.
            </div>
          </div>
        }
      />
    </>
  );
}
