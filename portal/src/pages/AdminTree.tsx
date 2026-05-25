import { useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { stringify, parse as parseYaml } from "yaml";

import { listObjects, applyObject, deleteObject, type KubeObject } from "../api/admin";
import { YamlEditor } from "../components/YamlEditor";
import { PageTitle } from "../components/PageTitle";
import { isApiError } from "../api/client";

// ─── Tree building helpers ────────────────────────────────────────────────────

interface OrgNode {
  name: string;
  namespace: string;
  apps: AppNode[];
}

interface AppNode {
  name: string;
  namespace: string;
  domains: DomainNode[];
}

interface DomainNode {
  name: string;
  namespace: string;
  schemas: KubeObject[];
}

function label(obj: KubeObject, key: string): string {
  return obj.metadata.labels?.[key] ?? "";
}

function buildTree(
  orgs: KubeObject[],
  apps: KubeObject[],
  domains: KubeObject[],
  schemas: KubeObject[],
): OrgNode[] {
  return orgs.map((org) => {
    const orgName = org.metadata.name;
    const orgNs = org.metadata.namespace ?? orgName;
    const orgApps = apps.filter((a) => label(a, "velocity.sh/org") === orgName);
    return {
      name: orgName,
      namespace: orgNs,
      apps: orgApps.map((app) => {
        const appName = app.metadata.name;
        const appNs = app.metadata.namespace ?? "";
        const appDomains = domains.filter(
          (d) =>
            label(d, "velocity.sh/org") === orgName &&
            label(d, "velocity.sh/app") === appName,
        );
        return {
          name: appName,
          namespace: appNs,
          domains: appDomains.map((domain) => {
            const domainName = domain.metadata.name;
            const domainNs = domain.metadata.namespace ?? "";
            const domainSchemas = schemas.filter(
              (s) =>
                label(s, "velocity.sh/org") === orgName &&
                label(s, "velocity.sh/app") === appName &&
                label(s, "velocity.sh/domain") === domainName,
            );
            return { name: domainName, namespace: domainNs, schemas: domainSchemas };
          }),
        };
      }),
    };
  });
}

// ─── Detail panel ─────────────────────────────────────────────────────────────

type Tab = "overview" | "yaml" | "edit";

function cleanForDisplay(obj: KubeObject): KubeObject {
  // Strip managed fields from display to keep YAML readable.
  const { metadata, ...rest } = obj;
  const { managedFields: _mf, ...cleanMeta } = metadata as Record<string, unknown>;
  return { metadata: cleanMeta as KubeObject["metadata"], ...rest };
}

interface DetailPanelProps {
  obj: KubeObject;
  kind: string;
  onClose: () => void;
  onDelete: () => void;
}

function DetailPanel({ obj, kind, onClose, onDelete }: DetailPanelProps) {
  const [tab, setTab] = useState<Tab>("overview");
  const [editYaml, setEditYaml] = useState(() => stringify(cleanForDisplay(obj)));
  const [applyError, setApplyError] = useState<string | null>(null);
  const [applySuccess, setApplySuccess] = useState(false);
  const queryClient = useQueryClient();

  const applyMut = useMutation({
    mutationFn: async () => {
      const parsed = parseYaml(editYaml) as KubeObject;
      const ns = parsed.metadata?.namespace ?? obj.metadata.namespace ?? "";
      const name = parsed.metadata?.name ?? obj.metadata.name;
      return applyObject(kind, ns, name, parsed);
    },
    onSuccess: () => {
      setApplySuccess(true);
      setApplyError(null);
      queryClient.invalidateQueries({ queryKey: ["admin", kind] });
      setTimeout(() => setApplySuccess(false), 3000);
    },
    onError: (e) => {
      setApplyError(isApiError(e) ? e.message : String(e));
    },
  });

  const displayYaml = stringify(cleanForDisplay(obj));
  const name = obj.metadata.name;
  const ns = obj.metadata.namespace;

  const tabs: { id: Tab; label: string }[] = [
    { id: "overview", label: "Overview" },
    { id: "yaml", label: "YAML" },
    { id: "edit", label: "Edit YAML" },
  ];

  return (
    <div className="flex flex-col h-full bg-ink-900 border-l border-ink-700">
      {/* Header */}
      <div className="flex items-center justify-between px-4 py-3 border-b border-ink-700 shrink-0">
        <div>
          <div className="text-xs text-ink-400 uppercase tracking-wider">{kind}</div>
          <div className="text-sm font-medium text-ink-100 mt-0.5">{name}</div>
          {ns && <div className="text-xs text-ink-400">{ns}</div>}
        </div>
        <div className="flex gap-2">
          <button
            onClick={onDelete}
            className="text-xs px-2 py-1 rounded bg-red-900 text-red-200 hover:bg-red-800"
          >
            Delete
          </button>
          <button
            onClick={onClose}
            className="text-xs px-2 py-1 rounded bg-ink-700 text-ink-200 hover:bg-ink-600"
          >
            ✕
          </button>
        </div>
      </div>

      {/* Tab strip */}
      <div className="flex border-b border-ink-700 shrink-0">
        {tabs.map((t) => (
          <button
            key={t.id}
            onClick={() => setTab(t.id)}
            className={[
              "px-4 py-2 text-xs",
              tab === t.id
                ? "text-amber-400 border-b-2 border-amber-500"
                : "text-ink-400 hover:text-ink-200",
            ].join(" ")}
          >
            {t.label}
          </button>
        ))}
      </div>

      {/* Content */}
      <div className="flex-1 overflow-auto">
        {tab === "overview" && (
          <div className="p-4 space-y-3 text-xs">
            <Row label="Kind" value={obj.kind ?? kind} />
            <Row label="Name" value={name} />
            {ns && <Row label="Namespace" value={ns} />}
            {obj.metadata.uid && <Row label="UID" value={obj.metadata.uid} />}
            {obj.metadata.creationTimestamp && (
              <Row label="Created" value={obj.metadata.creationTimestamp} />
            )}
            {obj.metadata.labels && Object.keys(obj.metadata.labels).length > 0 && (
              <div>
                <div className="text-ink-400 mb-1">Labels</div>
                <div className="space-y-1">
                  {Object.entries(obj.metadata.labels).map(([k, v]) => (
                    <div key={k} className="flex gap-2">
                      <span className="text-ink-400 shrink-0">{k}:</span>
                      <span className="text-ink-100 break-all">{v}</span>
                    </div>
                  ))}
                </div>
              </div>
            )}
            {!!obj.status && (
              <div>
                <div className="text-ink-400 mb-1">Status</div>
                <pre className="bg-ink-800 rounded p-2 text-ink-200 overflow-auto text-[11px]">
                  {JSON.stringify(obj.status, null, 2)}
                </pre>
              </div>
            )}
          </div>
        )}

        {tab === "yaml" && (
          <div className="h-full min-h-0" style={{ height: "calc(100% - 0px)" }}>
            <YamlEditor value={displayYaml} readOnly height="100%" />
          </div>
        )}

        {tab === "edit" && (
          <div className="flex flex-col h-full">
            <div className="flex-1 min-h-0">
              <YamlEditor value={editYaml} onChange={setEditYaml} height="100%" />
            </div>
            <div className="px-4 py-3 border-t border-ink-700 shrink-0 flex items-center gap-3">
              <button
                onClick={() => applyMut.mutate()}
                disabled={applyMut.isPending}
                className="text-xs px-3 py-1.5 rounded bg-amber-600 text-white hover:bg-amber-500 disabled:opacity-50"
              >
                {applyMut.isPending ? "Applying…" : "Apply to cluster"}
              </button>
              {applySuccess && (
                <span className="text-xs text-green-400">Applied successfully</span>
              )}
              {applyError && (
                <span className="text-xs text-red-400">{applyError}</span>
              )}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

function Row({ label: l, value }: { label: string; value: string }) {
  return (
    <div className="flex gap-2">
      <span className="text-ink-400 shrink-0 w-24">{l}</span>
      <span className="text-ink-100 break-all">{value}</span>
    </div>
  );
}

// ─── Delete confirm ──────────────────────────────────────────────────────────

interface ConfirmDeleteProps {
  kind: string;
  name: string;
  namespace: string;
  onConfirm: () => void;
  onCancel: () => void;
  isPending: boolean;
}

function ConfirmDelete({ kind, name, namespace, onConfirm, onCancel, isPending }: ConfirmDeleteProps) {
  return (
    <div className="fixed inset-0 bg-black/60 flex items-center justify-center z-50">
      <div className="bg-ink-900 border border-ink-700 rounded-lg p-6 max-w-sm w-full mx-4">
        <h3 className="text-sm font-medium text-ink-100 mb-2">Delete {kind}?</h3>
        <p className="text-xs text-ink-400 mb-4">
          <span className="text-ink-200">{name}</span> in <span className="text-ink-200">{namespace}</span> will be
          permanently removed from the cluster.
        </p>
        <div className="flex gap-2 justify-end">
          <button
            onClick={onCancel}
            className="text-xs px-3 py-1.5 rounded bg-ink-700 text-ink-200 hover:bg-ink-600"
          >
            Cancel
          </button>
          <button
            onClick={onConfirm}
            disabled={isPending}
            className="text-xs px-3 py-1.5 rounded bg-red-700 text-white hover:bg-red-600 disabled:opacity-50"
          >
            {isPending ? "Deleting…" : "Delete"}
          </button>
        </div>
      </div>
    </div>
  );
}

// ─── Tree navigation ──────────────────────────────────────────────────────────

interface SelectedObj {
  obj: KubeObject;
  kind: string;
}

export function AdminTree() {
  const [selected, setSelected] = useState<SelectedObj | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<SelectedObj | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const queryClient = useQueryClient();

  const orgsQ  = useQuery({ queryKey: ["admin", "Organisation"],     queryFn: () => listObjects("Organisation") });
  const appsQ  = useQuery({ queryKey: ["admin", "Application"],      queryFn: () => listObjects("Application") });
  const domsQ  = useQuery({ queryKey: ["admin", "Domain"],           queryFn: () => listObjects("Domain") });
  const schemasQ = useQuery({ queryKey: ["admin", "SchemaDefinition"], queryFn: () => listObjects("SchemaDefinition") });

  const loading = orgsQ.isLoading || appsQ.isLoading || domsQ.isLoading || schemasQ.isLoading;
  const error = orgsQ.error ?? appsQ.error ?? domsQ.error ?? schemasQ.error;

  const tree =
    !loading && !error
      ? buildTree(
          orgsQ.data?.items ?? [],
          appsQ.data?.items ?? [],
          domsQ.data?.items ?? [],
          schemasQ.data?.items ?? [],
        )
      : [];

  const deleteMut = useMutation({
    mutationFn: () => {
      if (!deleteTarget) throw new Error("no target");
      const ns = deleteTarget.obj.metadata.namespace ?? "";
      const name = deleteTarget.obj.metadata.name;
      return deleteObject(deleteTarget.kind, ns, name);
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["admin", deleteTarget?.kind] });
      if (selected?.obj === deleteTarget?.obj) setSelected(null);
      setDeleteTarget(null);
    },
  });

  function toggle(key: string) {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }

  function select(obj: KubeObject, kind: string) {
    setSelected({ obj, kind });
  }

  return (
    <div className="flex flex-col h-full">
      <PageTitle
        title="Admin"
        subtitle="Browse and edit cluster CRDs — Org → App → Domain → Schema"
      />

      <div className="flex flex-1 overflow-hidden">
        {/* Left: tree */}
        <div className="w-72 shrink-0 border-r border-ink-700 overflow-y-auto bg-ink-900">
          {loading && (
            <div className="px-4 py-6 text-xs text-ink-400">Loading…</div>
          )}
          {error && (
            <div className="px-4 py-6 text-xs text-red-400">
              {isApiError(error) ? error.message : "Failed to load objects"}
            </div>
          )}
          {!loading && !error && tree.length === 0 && (
            <div className="px-4 py-6 text-xs text-ink-400">No organisations found.</div>
          )}
          {tree.map((org) => (
            <div key={org.name}>
              <TreeRow
                label={org.name}
                depth={0}
                expandKey={`org:${org.name}`}
                expanded={expanded}
                onToggle={toggle}
                onClick={() => {
                  const obj = orgsQ.data?.items.find((o) => o.metadata.name === org.name);
                  if (obj) select(obj, "Organisation");
                }}
                isSelected={selected?.obj.metadata.name === org.name && selected.kind === "Organisation"}
              />
              {expanded.has(`org:${org.name}`) &&
                org.apps.map((app) => (
                  <div key={app.name}>
                    <TreeRow
                      label={app.name}
                      depth={1}
                      expandKey={`app:${org.name}/${app.name}`}
                      expanded={expanded}
                      onToggle={toggle}
                      onClick={() => {
                        const obj = appsQ.data?.items.find((o) => o.metadata.name === app.name && label(o, "velocity.sh/org") === org.name);
                        if (obj) select(obj, "Application");
                      }}
                      isSelected={selected?.obj.metadata.name === app.name && selected.kind === "Application"}
                    />
                    {expanded.has(`app:${org.name}/${app.name}`) &&
                      app.domains.map((dom) => (
                        <div key={dom.name}>
                          <TreeRow
                            label={dom.name}
                            depth={2}
                            expandKey={`dom:${org.name}/${app.name}/${dom.name}`}
                            expanded={expanded}
                            onToggle={toggle}
                            onClick={() => {
                              const obj = domsQ.data?.items.find((o) => o.metadata.name === dom.name && label(o, "velocity.sh/org") === org.name && label(o, "velocity.sh/app") === app.name);
                              if (obj) select(obj, "Domain");
                            }}
                            isSelected={selected?.obj.metadata.name === dom.name && selected.kind === "Domain"}
                          />
                          {expanded.has(`dom:${org.name}/${app.name}/${dom.name}`) &&
                            dom.schemas.map((sd) => (
                              <TreeRow
                                key={sd.metadata.name}
                                label={sd.metadata.name}
                                depth={3}
                                isLeaf
                                onClick={() => select(sd, "SchemaDefinition")}
                                isSelected={selected?.obj.metadata.name === sd.metadata.name && selected.kind === "SchemaDefinition"}
                              />
                            ))}
                        </div>
                      ))}
                  </div>
                ))}
            </div>
          ))}
        </div>

        {/* Right: detail */}
        {selected ? (
          <div className="flex-1 overflow-hidden">
            <DetailPanel
              obj={selected.obj}
              kind={selected.kind}
              onClose={() => setSelected(null)}
              onDelete={() => setDeleteTarget(selected)}
            />
          </div>
        ) : (
          <div className="flex-1 flex items-center justify-center text-xs text-ink-400">
            Select an object from the tree to view details.
          </div>
        )}
      </div>

      {/* Delete confirm modal */}
      {deleteTarget && (
        <ConfirmDelete
          kind={deleteTarget.kind}
          name={deleteTarget.obj.metadata.name}
          namespace={deleteTarget.obj.metadata.namespace ?? ""}
          onConfirm={() => deleteMut.mutate()}
          onCancel={() => setDeleteTarget(null)}
          isPending={deleteMut.isPending}
        />
      )}
    </div>
  );
}

// ─── Tree row component ───────────────────────────────────────────────────────

interface TreeRowProps {
  label: string;
  depth: number;
  isLeaf?: boolean;
  expandKey?: string;
  expanded?: Set<string>;
  onToggle?: (key: string) => void;
  onClick: () => void;
  isSelected: boolean;
}

function TreeRow({ label: lbl, depth, isLeaf, expandKey, expanded, onToggle, onClick, isSelected }: TreeRowProps) {
  const isExpanded = expandKey ? expanded?.has(expandKey) : false;
  const indent = depth * 16 + 12;

  return (
    <div
      onClick={() => {
        onClick();
        if (!isLeaf && expandKey && onToggle) onToggle(expandKey);
      }}
      className={[
        "flex items-center gap-1.5 py-1 cursor-pointer text-xs select-none",
        "hover:bg-ink-800",
        isSelected ? "bg-ink-800 text-amber-400" : "text-ink-200",
      ].join(" ")}
      style={{ paddingLeft: indent }}
    >
      {!isLeaf && (
        <span className="text-ink-500 w-3 shrink-0 text-center">
          {isExpanded ? "▾" : "▸"}
        </span>
      )}
      {isLeaf && <span className="w-3 shrink-0" />}
      <span className="truncate">{lbl}</span>
    </div>
  );
}
