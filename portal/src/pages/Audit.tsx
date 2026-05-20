import { useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { useSearchParams } from "react-router-dom";

import { endpoints } from "../api/endpoints";
import { Async } from "../components/Async";
import { PageTitle } from "../components/PageTitle";
import { isApiError } from "../api/client";

export function Audit() {
  const [params, setParams] = useSearchParams();
  const filters = {
    actor:     params.get("actor")     ?? "",
    schema:    params.get("schema")    ?? "",
    entity:    params.get("entity")    ?? "",
    operation: params.get("operation") ?? "",
    outcome:   params.get("outcome")   ?? "",
    limit:     Number(params.get("limit") ?? 50),
  };
  const [draft, setDraft] = useState(filters);

  const list = useQuery({
    queryKey: ["audit", filters],
    queryFn: () => endpoints.auditList(filters),
  });

  const verify = useMutation({
    mutationFn: endpoints.auditVerify,
  });

  const apply = () => {
    const next = new URLSearchParams();
    (Object.keys(draft) as (keyof typeof draft)[]).forEach((k) => {
      const v = draft[k];
      if (v !== "" && v !== undefined && v !== null && v !== 0) next.set(k, String(v));
    });
    setParams(next);
  };

  return (
    <>
      <PageTitle
        title="Audit log"
        subtitle="Append-only, hash-chained"
        actions={
          <button
            className="btn"
            disabled={verify.isPending}
            onClick={() => verify.mutate()}
          >
            {verify.isPending ? "verifying…" : "Verify chain"}
          </button>
        }
      />

      {verify.data && (
        <div
          className={`card p-3 text-xs mb-3 ${verify.data.ok ? "text-emerald-300" : "text-red-300"}`}
        >
          {verify.data.ok
            ? "audit chain OK"
            : `BREAK at id ${verify.data.first_break_id}: ${verify.data.message ?? ""}`}
        </div>
      )}
      {verify.error && (
        <div className="text-xs text-red-300 mb-3">
          {isApiError(verify.error) ? verify.error.message : String(verify.error)}
        </div>
      )}

      <div className="card p-3 mb-3 grid grid-cols-5 gap-2">
        <div>
          <label className="label">Actor</label>
          <input className="input" value={draft.actor} onChange={(e) => setDraft({ ...draft, actor: e.target.value })} />
        </div>
        <div>
          <label className="label">Schema</label>
          <input className="input" value={draft.schema} onChange={(e) => setDraft({ ...draft, schema: e.target.value })} />
        </div>
        <div>
          <label className="label">Entity ID</label>
          <input className="input" value={draft.entity} onChange={(e) => setDraft({ ...draft, entity: e.target.value })} />
        </div>
        <div>
          <label className="label">Operation</label>
          <input className="input" value={draft.operation} onChange={(e) => setDraft({ ...draft, operation: e.target.value })} />
        </div>
        <div>
          <label className="label">Outcome</label>
          <input className="input" value={draft.outcome} onChange={(e) => setDraft({ ...draft, outcome: e.target.value })} />
        </div>
        <div className="col-span-5 flex justify-end">
          <button className="btn btn-primary" onClick={apply}>
            Apply filters
          </button>
        </div>
      </div>

      <div className="card">
        <Async query={list}>
          {(data) =>
            data.items.length === 0 ? (
              <div className="text-xs text-ink-400 p-4">No entries match.</div>
            ) : (
              <table className="table">
                <thead>
                  <tr>
                    <th>ID</th>
                    <th>Time</th>
                    <th>Actor</th>
                    <th>Schema</th>
                    <th>Entity</th>
                    <th>Op</th>
                    <th>Outcome</th>
                    <th>Fail mode</th>
                  </tr>
                </thead>
                <tbody>
                  {data.items.map((e) => (
                    <tr key={e.id}>
                      <td className="text-ink-400">{e.id}</td>
                      <td className="text-ink-300">{new Date(e.timestamp).toLocaleString()}</td>
                      <td>{e.actor_id}</td>
                      <td>{e.schema_kind ?? "—"}</td>
                      <td className="font-mono">{e.entity_id ?? "—"}</td>
                      <td>{e.operation}</td>
                      <td>
                        <span className={`badge ${e.outcome === "success" ? "badge-ok" : "badge-err"}`}>
                          {e.outcome}
                        </span>
                      </td>
                      <td className="text-ink-300">{e.fail_mode ?? "—"}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )
          }
        </Async>
      </div>
    </>
  );
}
