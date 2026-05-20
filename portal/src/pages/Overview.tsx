import { useQuery } from "@tanstack/react-query";

import { endpoints } from "../api/endpoints";
import { Async } from "../components/Async";
import { PageTitle } from "../components/PageTitle";
import { parseSchemaPath } from "../api/types";

export function Overview() {
  const registry = useQuery({
    queryKey: ["registry"],
    queryFn: endpoints.registryIndex,
    refetchInterval: 15_000,
  });
  const audit = useQuery({
    queryKey: ["audit", "recent"],
    queryFn: () => endpoints.auditList({ limit: 5 }),
    refetchInterval: 30_000,
  });

  return (
    <>
      <PageTitle title="Overview" subtitle="Velocity cluster at a glance" />
      <div className="grid grid-cols-3 gap-3">
        <div className="card p-4">
          <div className="text-[11px] uppercase tracking-wider text-ink-400 mb-1">Schemas</div>
          <Async query={registry}>
            {(data) => <div className="text-3xl font-bold text-amber-400">{data.schemas}</div>}
          </Async>
        </div>
        <div className="card p-4">
          <div className="text-[11px] uppercase tracking-wider text-ink-400 mb-1">Registry</div>
          <Async query={registry}>
            {(data) => (
              <div className={`text-3xl font-bold ${data.ready ? "text-emerald-400" : "text-red-400"}`}>
                {data.ready ? "ready" : "starting"}
              </div>
            )}
          </Async>
        </div>
        <div className="card p-4">
          <div className="text-[11px] uppercase tracking-wider text-ink-400 mb-1">Service</div>
          <Async query={registry}>
            {(data) => <div className="text-xl font-mono text-ink-100">{data.service}</div>}
          </Async>
        </div>
      </div>

      <div className="card mt-4">
        <div className="panel-title">Recent audit activity</div>
        <Async query={audit}>
          {(data) =>
            data.items.length === 0 ? (
              <div className="text-xs text-ink-400 p-4">No recent entries.</div>
            ) : (
              <table className="table">
                <thead>
                  <tr>
                    <th>Time</th>
                    <th>Actor</th>
                    <th>Operation</th>
                    <th>Outcome</th>
                    <th>Schema</th>
                  </tr>
                </thead>
                <tbody>
                  {data.items.map((e) => (
                    <tr key={e.id}>
                      <td className="text-ink-300">{new Date(e.timestamp).toLocaleString()}</td>
                      <td>{e.actor_id}</td>
                      <td>{e.operation}</td>
                      <td>
                        <span className={`badge ${e.outcome === "success" ? "badge-ok" : "badge-err"}`}>
                          {e.outcome}
                        </span>
                      </td>
                      <td className="text-ink-300">{e.schema_kind ?? "—"}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )
          }
        </Async>
      </div>

      <div className="card mt-4">
        <div className="panel-title">Registered schemas</div>
        <Async query={registry}>
          {(data) =>
            data.paths.length === 0 ? (
              <div className="text-xs text-ink-400 p-4">No schemas registered.</div>
            ) : (
              <ul className="text-xs">
                {data.paths.slice(0, 20).map((p) => {
                  const sp = parseSchemaPath(p);
                  return (
                    <li key={p} className="px-3 py-1.5 border-b border-ink-800 hover:bg-ink-800">
                      <a
                        className="text-amber-400 hover:underline"
                        href={`/schemas/${sp.org}/${sp.app}/${sp.domain}/${sp.object}/${sp.version}`}
                      >
                        {p}
                      </a>
                    </li>
                  );
                })}
              </ul>
            )
          }
        </Async>
      </div>
    </>
  );
}
