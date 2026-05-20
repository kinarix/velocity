import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useParams } from "react-router-dom";

import { endpoints } from "../api/endpoints";
import { Async } from "../components/Async";
import { JsonEditor } from "../components/JsonEditor";
import { PageTitle } from "../components/PageTitle";
import { isApiError } from "../api/client";
import type { SchemaPath } from "../api/types";

export function Records() {
  const p = useParams<keyof SchemaPath>() as SchemaPath;
  const path = `${p.org}/${p.app}/${p.domain}/${p.object}/${p.version}`;
  const qc = useQueryClient();

  const list = useQuery({
    queryKey: ["records", path],
    queryFn: () => endpoints.listRecords(p, { limit: 50 }),
  });

  const [showCreate, setShowCreate] = useState(false);
  const [draft, setDraft] = useState<unknown>({});
  const [createErr, setCreateErr] = useState<string | null>(null);

  const create = useMutation({
    mutationFn: (body: unknown) => endpoints.createRecord(p, body),
    onSuccess: () => {
      setShowCreate(false);
      setDraft({});
      setCreateErr(null);
      void qc.invalidateQueries({ queryKey: ["records", path] });
    },
    onError: (e) => {
      setCreateErr(isApiError(e) ? `${e.status} ${e.message}` : String(e));
    },
  });

  return (
    <>
      <PageTitle
        title={`${p.object} · records`}
        subtitle={path}
        actions={
          <>
            <Link to={`/schemas/${path}`} className="btn">
              Schema
            </Link>
            <button className="btn btn-primary" onClick={() => setShowCreate((v) => !v)}>
              {showCreate ? "Cancel" : "+ New record"}
            </button>
          </>
        }
      />

      {showCreate && (
        <div className="card p-4 mb-4">
          <div className="panel-title -m-4 mb-3">New record (JSON body)</div>
          <JsonEditor value={draft} onChange={setDraft} rows={12} />
          {createErr && <div className="text-xs text-red-300 mt-2">{createErr}</div>}
          <div className="flex justify-end gap-2 mt-2">
            <button className="btn" onClick={() => setShowCreate(false)}>
              Cancel
            </button>
            <button
              className="btn btn-primary"
              disabled={create.isPending}
              onClick={() => create.mutate(draft)}
            >
              {create.isPending ? "creating…" : "Create"}
            </button>
          </div>
        </div>
      )}

      <div className="card">
        <Async query={list}>
          {(data) =>
            data.items.length === 0 ? (
              <div className="text-xs text-ink-400 p-4">No records.</div>
            ) : (
              <table className="table">
                <thead>
                  <tr>
                    <th>ID</th>
                    <th>Version</th>
                    <th>Updated</th>
                    <th>Updated by</th>
                    <th></th>
                  </tr>
                </thead>
                <tbody>
                  {data.items.map((r) => (
                    <tr key={r.id}>
                      <td className="font-mono text-amber-400">{r.id}</td>
                      <td>{r.version}</td>
                      <td className="text-ink-300">{new Date(r.updated_at).toLocaleString()}</td>
                      <td className="text-ink-300">{r.updated_by ?? "—"}</td>
                      <td className="text-right">
                        <Link className="btn btn-ghost" to={`/records/${path}/${r.id}`}>
                          view
                        </Link>
                        <Link className="btn btn-ghost ml-1" to={`/records/${path}/${r.id}/history`}>
                          history
                        </Link>
                      </td>
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
