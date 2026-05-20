import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useParams } from "react-router-dom";

import { endpoints } from "../api/endpoints";
import { Async } from "../components/Async";
import { PageTitle } from "../components/PageTitle";
import { isApiError } from "../api/client";
import type { SchemaPath } from "../api/types";

export function History() {
  const p = useParams<keyof SchemaPath | "id">() as SchemaPath & { id: string };
  const path = `${p.org}/${p.app}/${p.domain}/${p.object}/${p.version}`;
  const qc = useQueryClient();

  const hist = useQuery({
    queryKey: ["history", path, p.id],
    queryFn: () => endpoints.recordHistory(p, p.id),
  });

  const [from, setFrom] = useState<number | null>(null);
  const [to, setTo] = useState<number | null>(null);

  const diff = useQuery({
    queryKey: ["diff", path, p.id, from, to],
    queryFn: () => endpoints.recordDiff(p, p.id, from!, to!),
    enabled: from !== null && to !== null,
  });

  const restore = useMutation({
    mutationFn: (v: number) => endpoints.restoreRecord(p, p.id, v),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["record", path, p.id] });
      void qc.invalidateQueries({ queryKey: ["history", path, p.id] });
    },
  });

  return (
    <>
      <PageTitle
        title={`History · ${p.id}`}
        subtitle={path}
        actions={
          <Link to={`/records/${path}/${p.id}`} className="btn">
            Back to record
          </Link>
        }
      />

      <div className="grid grid-cols-2 gap-3">
        <div className="card">
          <div className="panel-title">Versions</div>
          <Async query={hist}>
            {(data) =>
              data.items.length === 0 ? (
                <div className="text-xs text-ink-400 p-4">No history.</div>
              ) : (
                <table className="table">
                  <thead>
                    <tr>
                      <th>Ver</th>
                      <th>When</th>
                      <th>Actor</th>
                      <th>Op</th>
                      <th>Compare</th>
                      <th></th>
                    </tr>
                  </thead>
                  <tbody>
                    {data.items.map((h) => (
                      <tr key={h.version}>
                        <td className="font-mono">{h.version}</td>
                        <td className="text-ink-300">{new Date(h.timestamp).toLocaleString()}</td>
                        <td className="text-ink-300">{h.actor_id ?? "—"}</td>
                        <td>{h.operation}</td>
                        <td className="space-x-1">
                          <button
                            className={`btn btn-ghost ${from === h.version ? "border-amber-500 text-amber-400" : ""}`}
                            onClick={() => setFrom(h.version)}
                          >
                            from
                          </button>
                          <button
                            className={`btn btn-ghost ${to === h.version ? "border-amber-500 text-amber-400" : ""}`}
                            onClick={() => setTo(h.version)}
                          >
                            to
                          </button>
                        </td>
                        <td>
                          <button
                            className="btn btn-ghost"
                            disabled={restore.isPending}
                            onClick={() => {
                              if (confirm(`Restore to v${h.version}?`)) restore.mutate(h.version);
                            }}
                          >
                            restore
                          </button>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              )
            }
          </Async>
        </div>

        <div className="card">
          <div className="panel-title">
            Diff {from !== null && to !== null ? `v${from} → v${to}` : ""}
          </div>
          {from === null || to === null ? (
            <div className="text-xs text-ink-400 p-4">Select <code>from</code> and <code>to</code> versions to view the diff.</div>
          ) : (
            <Async query={diff}>
              {(data) => (
                <pre className="text-xs leading-5 p-3 overflow-x-auto">
                  <code>{JSON.stringify(data, null, 2)}</code>
                </pre>
              )}
            </Async>
          )}
        </div>
      </div>

      {restore.error && (
        <div className="text-xs text-red-300 mt-2">
          restore failed: {isApiError(restore.error) ? restore.error.message : String(restore.error)}
        </div>
      )}
    </>
  );
}
