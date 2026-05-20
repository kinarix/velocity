import { useQuery } from "@tanstack/react-query";

import { endpoints } from "../api/endpoints";
import { api } from "../api/client";
import { Async } from "../components/Async";
import { PageTitle } from "../components/PageTitle";
import { parseSchemaPath } from "../api/types";

interface Probe {
  ready: boolean;
  body: string;
}

async function probe(): Promise<Probe> {
  // velocity-api ships /healthz and /readyz. /readyz only flips after the
  // schema-registry informer has received its first Restarted event.
  try {
    const body = await api.get<string>("/readyz");
    return { ready: true, body: typeof body === "string" ? body : JSON.stringify(body) };
  } catch (e) {
    return { ready: false, body: String(e) };
  }
}

export function Health() {
  const health = useQuery({
    queryKey: ["health"],
    queryFn: probe,
    refetchInterval: 10_000,
  });
  const registry = useQuery({
    queryKey: ["registry"],
    queryFn: endpoints.registryIndex,
  });

  return (
    <>
      <PageTitle title="Health" subtitle="API readiness + per-schema status" />

      <div className="card p-4 mb-3">
        <div className="panel-title -m-4 mb-3">API server</div>
        <Async query={health}>
          {(data) => (
            <div className="flex items-center gap-3">
              <span className={`badge ${data.ready ? "badge-ok" : "badge-err"}`}>
                {data.ready ? "ready" : "not ready"}
              </span>
              <code className="text-[11px] text-ink-300">{data.body}</code>
            </div>
          )}
        </Async>
      </div>

      <div className="card">
        <div className="panel-title">Schemas</div>
        <Async query={registry}>
          {(data) =>
            data.paths.length === 0 ? (
              <div className="text-xs text-ink-400 p-4">No schemas.</div>
            ) : (
              <table className="table">
                <thead>
                  <tr>
                    <th>Path</th>
                    <th>Status</th>
                  </tr>
                </thead>
                <tbody>
                  {data.paths.map((p) => {
                    const sp = parseSchemaPath(p);
                    return (
                      <tr key={p}>
                        <td className="font-mono text-amber-400">
                          {sp.org}/{sp.app}/{sp.domain}/{sp.object}/{sp.version}
                        </td>
                        <td>
                          <span className="badge badge-ok">registered</span>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            )
          }
        </Async>
        <div className="text-[11px] text-ink-400 p-3 border-t border-ink-800">
          Per-schema reconcile and migration status is exposed on the
          <code className="text-amber-400"> SchemaDefinition</code> CRD itself —
          fetch with <code>velocity get schemadefinition &lt;name&gt; -o yaml</code>.
        </div>
      </div>
    </>
  );
}
