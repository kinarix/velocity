import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router-dom";

import { endpoints } from "../api/endpoints";
import { Async } from "../components/Async";
import { PageTitle } from "../components/PageTitle";
import { parseSchemaPath } from "../api/types";

interface TreeNode {
  name: string;
  children: Map<string, TreeNode>;
  leafPath?: string;
}

function buildTree(paths: string[]): TreeNode {
  const root: TreeNode = { name: "", children: new Map() };
  for (const p of paths) {
    const sp = parseSchemaPath(p);
    const segs = [sp.org, sp.app, sp.domain, `${sp.object}/${sp.version}`];
    let cur = root;
    segs.forEach((seg, i) => {
      let next = cur.children.get(seg);
      if (!next) {
        next = { name: seg, children: new Map() };
        cur.children.set(seg, next);
      }
      if (i === segs.length - 1) next.leafPath = p;
      cur = next;
    });
  }
  return root;
}

function TreeBranch({ node, depth }: { node: TreeNode; depth: number }) {
  const children = Array.from(node.children.values());
  if (children.length === 0) return null;
  return (
    <ul className={depth === 0 ? "" : "ml-4 border-l border-ink-700 pl-3"}>
      {children.map((c) => (
        <li key={c.name} className="py-0.5">
          {c.leafPath ? (
            <Link
              to={`/schemas/${c.leafPath}`}
              className="text-amber-400 hover:underline text-xs"
            >
              {c.name}
            </Link>
          ) : (
            <span className="text-ink-200 text-xs">{c.name}</span>
          )}
          <TreeBranch node={c} depth={depth + 1} />
        </li>
      ))}
    </ul>
  );
}

export function Hierarchy() {
  const registry = useQuery({
    queryKey: ["registry"],
    queryFn: endpoints.registryIndex,
  });

  const tree = useMemo(() => (registry.data ? buildTree(registry.data.paths) : null), [registry.data]);

  return (
    <>
      <PageTitle title="Hierarchy" subtitle="Schemas grouped by org → app → domain → object/version" />
      <div className="card p-4">
        <Async query={registry}>
          {() =>
            tree ? <TreeBranch node={tree} depth={0} /> : <div className="text-xs text-ink-400">No schemas.</div>
          }
        </Async>
      </div>
    </>
  );
}
