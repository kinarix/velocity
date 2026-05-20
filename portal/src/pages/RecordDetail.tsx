import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "react-router-dom";

import { endpoints } from "../api/endpoints";
import { Async } from "../components/Async";
import { JsonEditor } from "../components/JsonEditor";
import { PageTitle } from "../components/PageTitle";
import { isApiError } from "../api/client";
import type { SchemaPath } from "../api/types";

export function RecordDetail() {
  const p = useParams<keyof SchemaPath | "id">() as SchemaPath & { id: string };
  const path = `${p.org}/${p.app}/${p.domain}/${p.object}/${p.version}`;
  const qc = useQueryClient();
  const nav = useNavigate();

  const rec = useQuery({
    queryKey: ["record", path, p.id],
    queryFn: () => endpoints.getRecord(p, p.id),
  });

  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState<unknown>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    if (rec.data && draft === null) setDraft(rec.data);
  }, [rec.data, draft]);

  const update = useMutation({
    mutationFn: (body: unknown) => endpoints.updateRecord(p, p.id, body),
    onSuccess: () => {
      setEditing(false);
      setErr(null);
      void qc.invalidateQueries({ queryKey: ["record", path, p.id] });
    },
    onError: (e) => setErr(isApiError(e) ? `${e.status} ${e.message}` : String(e)),
  });

  const del = useMutation({
    mutationFn: () => endpoints.deleteRecord(p, p.id),
    onSuccess: () => nav(`/records/${path}`),
    onError: (e) => setErr(isApiError(e) ? `${e.status} ${e.message}` : String(e)),
  });

  return (
    <>
      <PageTitle
        title={`Record ${p.id}`}
        subtitle={path}
        actions={
          <>
            <Link to={`/records/${path}/${p.id}/history`} className="btn">
              History
            </Link>
            {!editing ? (
              <button className="btn" onClick={() => setEditing(true)}>
                Edit
              </button>
            ) : (
              <button className="btn" onClick={() => setEditing(false)}>
                Cancel
              </button>
            )}
            <button
              className="btn"
              disabled={del.isPending}
              onClick={() => {
                if (confirm("Soft-delete this record?")) del.mutate();
              }}
            >
              {del.isPending ? "deleting…" : "Delete"}
            </button>
          </>
        }
      />

      <div className="card p-4">
        <Async query={rec}>
          {(data) =>
            editing ? (
              <>
                <JsonEditor value={draft ?? data} onChange={setDraft} rows={24} />
                {err && <div className="text-xs text-red-300 mt-2">{err}</div>}
                <div className="flex justify-end gap-2 mt-2">
                  <button
                    className="btn btn-primary"
                    disabled={update.isPending}
                    onClick={() => update.mutate(draft ?? data)}
                  >
                    {update.isPending ? "saving…" : "Save"}
                  </button>
                </div>
              </>
            ) : (
              <pre className="text-xs leading-5 overflow-x-auto">
                <code>{JSON.stringify(data, null, 2)}</code>
              </pre>
            )
          }
        </Async>
      </div>
    </>
  );
}
