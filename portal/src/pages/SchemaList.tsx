import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router-dom";

import { endpoints } from "../api/endpoints";
import { Async } from "../components/Async";
import { PageTitle } from "../components/PageTitle";
import { parseSchemaPath } from "../api/types";

export function SchemaList() {
  const registry = useQuery({
    queryKey: ["registry"],
    queryFn: endpoints.registryIndex,
  });
  const [filter, setFilter] = useState("");

  return (
    <>
      <PageTitle
        title="Schemas"
        subtitle="All SchemaDefinitions visible to this API server"
        actions={
          <Link to="/schemas/new" className="btn btn-primary">
            + New schema
          </Link>
        }
      />
      <div className="mb-3">
        <input
          className="input max-w-md"
          placeholder="filter by path…"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
      </div>
      <div className="card">
        <Async query={registry}>
          {(data) => {
            const filtered = data.paths
              .filter((p) => p.toLowerCase().includes(filter.toLowerCase()))
              .sort();
            return filtered.length === 0 ? (
              <div className="text-xs text-ink-400 p-4">No schemas match.</div>
            ) : (
              <table className="table">
                <thead>
                  <tr>
                    <th>Org</th>
                    <th>App</th>
                    <th>Domain</th>
                    <th>Object</th>
                    <th>Version</th>
                    <th></th>
                  </tr>
                </thead>
                <tbody>
                  {filtered.map((p) => {
                    const sp = parseSchemaPath(p);
                    return (
                      <tr key={p}>
                        <td>{sp.org}</td>
                        <td>{sp.app}</td>
                        <td>{sp.domain}</td>
                        <td className="font-medium">{sp.object}</td>
                        <td>
                          <span className="badge">{sp.version}</span>
                        </td>
                        <td className="text-right">
                          <Link className="btn btn-ghost" to={`/schemas/${p}`}>
                            details
                          </Link>
                          <Link className="btn btn-ghost ml-1" to={`/records/${p}`}>
                            records
                          </Link>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            );
          }}
        </Async>
      </div>
    </>
  );
}
