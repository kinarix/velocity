import { useMemo, useState } from "react";

import { PageTitle } from "../components/PageTitle";
import { SplitEditor } from "../components/SplitEditor";
import { applyObject, type KubeObject } from "../api/admin";

type FieldKind =
  | "string"
  | "int"
  | "float"
  | "bool"
  | "date"
  | "datetime"
  | "uuid"
  | "json"
  | "ref";

interface FieldRow {
  name: string;
  kind: FieldKind;
  required: boolean;
  unique: boolean;
  filterable: boolean;
  sortable: boolean;
  indexed: boolean;
  sensitivity?: "" | "pii" | "financial" | "confidential";
}

const blankField = (): FieldRow => ({
  name: "",
  kind: "string",
  required: false,
  unique: false,
  filterable: true,
  sortable: false,
  indexed: false,
  sensitivity: "",
});

export function SchemaEditor() {
  const [org, setOrg] = useState("acme");
  const [app, setApp] = useState("supply-chain");
  const [domain, setDomain] = useState("procurement");
  const [object, setObject] = useState("purchase-order");
  const [version, setVersion] = useState("v1");
  const [searchTier, setSearchTier] = useState<1 | 2 | 3>(2);
  const [authStrategy, setAuthStrategy] = useState("velocity-system/default");
  const [archivePolicy, setArchivePolicy] = useState("");
  const [logFilterPolicy, setLogFilterPolicy] = useState("");

  const [fields, setFields] = useState<FieldRow[]>([
    { name: "po_number",     kind: "string", required: true,  unique: true,  filterable: true,  sortable: true,  indexed: true,  sensitivity: "" },
    { name: "supplier_code", kind: "string", required: true,  unique: false, filterable: true,  sortable: false, indexed: false, sensitivity: "" },
    { name: "total_value",   kind: "float",  required: false, unique: false, filterable: true,  sortable: true,  indexed: false, sensitivity: "financial" },
  ]);

  const [validationCel, setValidationCel] = useState(
    'self.total_value >= 0 && self.po_number.startsWith("PO-")',
  );
  const [validationMsg, setValidationMsg] = useState("total_value must be non-negative; po_number must start with PO-");

  const updateField = (i: number, patch: Partial<FieldRow>) =>
    setFields((rows) => rows.map((r, ix) => (ix === i ? { ...r, ...patch } : r)));
  const addField = () => setFields((rows) => [...rows, blankField()]);
  const removeField = (i: number) => setFields((rows) => rows.filter((_, ix) => ix !== i));

  const value = useMemo(() => {
    const namespace = `${org}-${app}-${domain}`;
    return {
      apiVersion: "velocity.sh/v1",
      kind: "SchemaDefinition",
      metadata: {
        name: object,
        namespace,
        labels: {
          "velocity.sh/org": org,
          "velocity.sh/app": app,
          "velocity.sh/domain": domain,
          "velocity.sh/version": version,
        },
      },
      spec: {
        org,
        app,
        domain,
        object,
        version,
        access: { authStrategy },
        ...(archivePolicy ? { archive: { policy: archivePolicy } } : {}),
        ...(logFilterPolicy ? { logging: { filterPolicy: logFilterPolicy } } : {}),
        search: { tier: searchTier },
        fields: fields
          .filter((f) => f.name.trim().length > 0)
          .map((f) => ({
            name: f.name,
            kind: f.kind,
            ...(f.required ? { required: true } : {}),
            ...(f.unique ? { unique: true } : {}),
            ...(f.filterable ? { filterable: true } : {}),
            ...(f.sortable ? { sortable: true } : {}),
            ...(f.indexed ? { indexed: true } : {}),
            ...(f.sensitivity ? { sensitivity: f.sensitivity } : {}),
          })),
        ...(validationCel.trim()
          ? {
              validations: [
                {
                  cel: { rule: validationCel, message: validationMsg, maxExecutionMs: 10 },
                },
              ],
            }
          : {}),
      },
    };
  }, [
    org, app, domain, object, version, authStrategy, archivePolicy,
    logFilterPolicy, searchTier, fields, validationCel, validationMsg,
  ]);

  return (
    <>
      <PageTitle
        title="SchemaDefinition"
        subtitle="Visual editor. Live YAML preview on the right. Apply via velocity CLI."
      />
      <SplitEditor
        filename={`${object}.${version}.yaml`}
        value={value}
        onApply={() => applyObject("SchemaDefinition", `${org}-${app}-${domain}`, object, value as KubeObject)}
        form={
          <div className="space-y-4 text-xs">
            <section>
              <div className="panel-title -mx-4 mb-3 bg-ink-800">Identity</div>
              <div className="grid grid-cols-2 gap-2">
                <div>
                  <label className="label">Org</label>
                  <input className="input" value={org} onChange={(e) => setOrg(e.target.value)} />
                </div>
                <div>
                  <label className="label">App</label>
                  <input className="input" value={app} onChange={(e) => setApp(e.target.value)} />
                </div>
                <div>
                  <label className="label">Domain</label>
                  <input className="input" value={domain} onChange={(e) => setDomain(e.target.value)} />
                </div>
                <div>
                  <label className="label">Object</label>
                  <input className="input" value={object} onChange={(e) => setObject(e.target.value)} />
                </div>
                <div>
                  <label className="label">Version</label>
                  <input className="input" value={version} onChange={(e) => setVersion(e.target.value)} />
                </div>
                <div>
                  <label className="label">Search tier</label>
                  <select
                    className="input"
                    value={searchTier}
                    onChange={(e) => setSearchTier(Number(e.target.value) as 1 | 2 | 3)}
                  >
                    <option value={1}>1 — none</option>
                    <option value={2}>2 — postgres trigram</option>
                    <option value={3}>3 — typesense (outbox CDC)</option>
                  </select>
                </div>
              </div>
            </section>

            <section>
              <div className="panel-title -mx-4 mb-3 bg-ink-800">Policy references</div>
              <div className="grid grid-cols-1 gap-2">
                <div>
                  <label className="label">Auth strategy (namespace/name)</label>
                  <input className="input" value={authStrategy} onChange={(e) => setAuthStrategy(e.target.value)} />
                </div>
                <div>
                  <label className="label">Archive policy (optional)</label>
                  <input className="input" value={archivePolicy} onChange={(e) => setArchivePolicy(e.target.value)} />
                </div>
                <div>
                  <label className="label">Log filter policy (optional)</label>
                  <input className="input" value={logFilterPolicy} onChange={(e) => setLogFilterPolicy(e.target.value)} />
                </div>
              </div>
            </section>

            <section>
              <div className="panel-title -mx-4 mb-3 bg-ink-800 flex justify-between items-center">
                <span>Fields</span>
                <button className="btn btn-ghost" onClick={addField}>
                  + Add field
                </button>
              </div>
              <table className="table">
                <thead>
                  <tr>
                    <th>Name</th>
                    <th>Kind</th>
                    <th>Req</th>
                    <th>Uniq</th>
                    <th>Filt</th>
                    <th>Sort</th>
                    <th>Idx</th>
                    <th>Sensitivity</th>
                    <th></th>
                  </tr>
                </thead>
                <tbody>
                  {fields.map((f, i) => (
                    <tr key={i}>
                      <td>
                        <input
                          className="input"
                          value={f.name}
                          onChange={(e) => updateField(i, { name: e.target.value })}
                        />
                      </td>
                      <td>
                        <select
                          className="input"
                          value={f.kind}
                          onChange={(e) => updateField(i, { kind: e.target.value as FieldKind })}
                        >
                          {(["string", "int", "float", "bool", "date", "datetime", "uuid", "json", "ref"] as FieldKind[]).map((k) => (
                            <option key={k}>{k}</option>
                          ))}
                        </select>
                      </td>
                      {(["required", "unique", "filterable", "sortable", "indexed"] as const).map((k) => (
                        <td key={k}>
                          <input
                            type="checkbox"
                            checked={f[k]}
                            onChange={(e) => updateField(i, { [k]: e.target.checked } as Partial<FieldRow>)}
                          />
                        </td>
                      ))}
                      <td>
                        <select
                          className="input"
                          value={f.sensitivity ?? ""}
                          onChange={(e) =>
                            updateField(i, { sensitivity: e.target.value as FieldRow["sensitivity"] })
                          }
                        >
                          <option value="">—</option>
                          <option value="pii">pii</option>
                          <option value="financial">financial</option>
                          <option value="confidential">confidential</option>
                        </select>
                      </td>
                      <td>
                        <button className="btn btn-ghost" onClick={() => removeField(i)}>
                          ✕
                        </button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </section>

            <section>
              <div className="panel-title -mx-4 mb-3 bg-ink-800">Validation (CEL)</div>
              <label className="label">Rule</label>
              <input className="input font-mono" value={validationCel} onChange={(e) => setValidationCel(e.target.value)} />
              <label className="label mt-2">Message</label>
              <input className="input" value={validationMsg} onChange={(e) => setValidationMsg(e.target.value)} />
              <div className="text-[11px] text-ink-400 mt-1">
                Compiled at schema load. Max 10ms per evaluation (ADR / CLAUDE.md §CEL Patterns).
              </div>
            </section>
          </div>
        }
      />
    </>
  );
}
