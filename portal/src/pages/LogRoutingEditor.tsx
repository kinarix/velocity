import { useState } from "react";

import { PageTitle } from "../components/PageTitle";
import { SplitEditor } from "../components/SplitEditor";

export function LogRoutingEditor() {
  const [name, setName] = useState("default-routing");
  const [namespace, setNamespace] = useState("velocity-system");
  const [routesText, setRoutesText] = useState(
    `- match:
    severity: ">=warn"
  sinks:
    - kind: "kafka"
      topic: "velocity.logs.warn"
- match:
    schemaKind: "PurchaseOrder"
  sinks:
    - kind: "s3"
      bucket: "acme-archive"
      prefix: "logs/purchase-order/"`,
  );

  const value = {
    apiVersion: "velocity.sh/v1",
    kind: "LogRoutingPolicy",
    metadata: { name, namespace },
    spec: {
      routesYaml: routesText,
    },
  };

  return (
    <>
      <PageTitle
        title="New LogRoutingPolicy"
        subtitle="Per-tenant fan-out of enriched log events to Kafka/S3/HTTP sinks."
      />
      <SplitEditor
        filename={`${name}.logrouting.yaml`}
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
              <label className="label">Routes (YAML)</label>
              <textarea
                rows={16}
                className="input font-mono"
                value={routesText}
                onChange={(e) => setRoutesText(e.target.value)}
              />
            </div>
          </div>
        }
      />
    </>
  );
}
