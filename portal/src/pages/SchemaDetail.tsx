import { Link, useParams } from "react-router-dom";

import { PageTitle } from "../components/PageTitle";
import type { SchemaPath } from "../api/types";

/**
 * The API server does not currently expose per-schema introspection
 * (fields, indexes, access rules) — that info lives in the SchemaDefinition
 * CRD itself, fetched via `velocity get` / kubectl. We surface the path
 * and link out to records / history / audit so the page is still useful;
 * a richer detail view is gated on a future /api/{path}/_schema endpoint.
 */
export function SchemaDetail() {
  const p = useParams<keyof SchemaPath>() as SchemaPath;
  const path = `${p.org}/${p.app}/${p.domain}/${p.object}/${p.version}`;
  return (
    <>
      <PageTitle
        title={`${p.object} · ${p.version}`}
        subtitle={path}
        actions={
          <Link to={`/records/${path}`} className="btn btn-primary">
            Browse records
          </Link>
        }
      />

      <div className="grid grid-cols-2 gap-3">
        <div className="card p-4">
          <div className="panel-title -m-4 mb-3">Identity</div>
          <dl className="text-xs space-y-1">
            {(["org", "app", "domain", "object", "version"] as const).map((k) => (
              <div key={k} className="flex gap-2">
                <dt className="w-20 text-ink-400">{k}</dt>
                <dd className="text-ink-100">{p[k]}</dd>
              </div>
            ))}
          </dl>
        </div>

        <div className="card p-4">
          <div className="panel-title -m-4 mb-3">Quick links</div>
          <ul className="text-xs space-y-1.5">
            <li>
              <Link className="text-amber-400 hover:underline" to={`/records/${path}`}>
                Records →
              </Link>
            </li>
            <li>
              <Link className="text-amber-400 hover:underline" to={`/audit?schema=${p.object}`}>
                Audit log for this schema →
              </Link>
            </li>
            <li>
              <Link className="text-amber-400 hover:underline" to={`/schemas/${path}/edit`}>
                Edit schema (YAML) →
              </Link>
            </li>
          </ul>
        </div>
      </div>

      <div className="card mt-4 p-4">
        <div className="panel-title -m-4 mb-3">Full schema</div>
        <p className="text-xs text-ink-300">
          The full <code className="text-amber-400">SchemaDefinition</code> spec is not exposed by the
          REST API — fetch it via the CLI or kubectl:
        </p>
        <pre className="mt-2 p-2 text-xs bg-ink-950 rounded border border-ink-700 overflow-x-auto">
          <code>{`velocity get schemadefinition ${p.object} -n ${p.org}-${p.app}-${p.domain} -o yaml`}</code>
        </pre>
      </div>
    </>
  );
}
