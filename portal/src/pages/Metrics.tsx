import { useQuery } from "@tanstack/react-query";

import { endpoints } from "../api/endpoints";
import { Async } from "../components/Async";
import { PageTitle } from "../components/PageTitle";
import { useAuth } from "../auth/useAuth";
import { parseSchemaPath } from "../api/types";

export function Metrics() {
  const { config } = useAuth();
  const registry = useQuery({
    queryKey: ["registry"],
    queryFn: endpoints.registryIndex,
  });

  return (
    <>
      <PageTitle
        title="Metrics"
        subtitle="Per-schema dashboards live in Grafana. The operator provisions them automatically."
      />

      {config.grafana_url ? (
        <div className="card p-3 mb-3 text-xs">
          Open the central dashboard:&nbsp;
          <a
            className="text-amber-400 hover:underline"
            href={config.grafana_url}
            target="_blank"
            rel="noreferrer"
          >
            {config.grafana_url}
          </a>
        </div>
      ) : (
        <div className="card p-3 mb-3 text-xs text-ink-400">
          Configure <code>grafana_url</code> in <code>/config.json</code> to enable dashboard links.
        </div>
      )}

      <div className="card">
        <div className="panel-title">Per-schema dashboards</div>
        <Async query={registry}>
          {(data) => (
            <table className="table">
              <thead>
                <tr>
                  <th>Schema</th>
                  <th>Dashboard</th>
                </tr>
              </thead>
              <tbody>
                {data.paths.map((p) => {
                  const sp = parseSchemaPath(p);
                  const dashSlug = `${sp.org}-${sp.app}-${sp.domain}-${sp.object}-${sp.version}`;
                  const href = config.grafana_url
                    ? `${config.grafana_url.replace(/\/$/, "")}/d/${dashSlug}`
                    : null;
                  return (
                    <tr key={p}>
                      <td className="font-mono">{p}</td>
                      <td>
                        {href ? (
                          <a className="text-amber-400 hover:underline" href={href} target="_blank" rel="noreferrer">
                            open →
                          </a>
                        ) : (
                          <span className="text-ink-400">grafana_url not configured</span>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          )}
        </Async>
      </div>
    </>
  );
}
