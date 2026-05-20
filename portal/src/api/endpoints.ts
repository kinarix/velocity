import { api } from "./client";
import type {
  AuditEntry,
  AuditVerifyResponse,
  HistoryEntry,
  ListResponse,
  PortalConfig,
  RegistryIndex,
  SchemaPath,
  VelocityRecord,
} from "./types";

const basePath = (p: SchemaPath) =>
  `/api/${p.org}/${p.app}/${p.domain}/${p.object}/${p.version}`;

export const endpoints = {
  registryIndex: () => api.get<RegistryIndex>("/api"),

  // ---- records --------------------------------------------------------
  listRecords: (p: SchemaPath, q?: { limit?: number; cursor?: string }) => {
    const params = new URLSearchParams();
    if (q?.limit)  params.set("limit", String(q.limit));
    if (q?.cursor) params.set("cursor", q.cursor);
    const qs = params.toString();
    return api.get<ListResponse>(qs ? `${basePath(p)}?${qs}` : basePath(p));
  },
  getRecord:    (p: SchemaPath, id: string)        => api.get<VelocityRecord>(`${basePath(p)}/${id}`),
  createRecord: (p: SchemaPath, body: unknown)     => api.post<VelocityRecord>(basePath(p), body),
  updateRecord: (p: SchemaPath, id: string, body: unknown) =>
    api.put<VelocityRecord>(`${basePath(p)}/${id}`, body),
  deleteRecord: (p: SchemaPath, id: string)        => api.delete<void>(`${basePath(p)}/${id}`),
  queryRecords: (p: SchemaPath, body: unknown)     => api.post<ListResponse>(`${basePath(p)}/query`, body),
  searchRecords: (p: SchemaPath, body: unknown)    => api.post<ListResponse>(`${basePath(p)}/search`, body),

  // ---- time machine ---------------------------------------------------
  recordHistory: (p: SchemaPath, id: string) =>
    api.get<ListResponse<HistoryEntry>>(`${basePath(p)}/${id}/history`),
  recordDiff: (p: SchemaPath, id: string, from: number, to: number) =>
    api.get<unknown>(`${basePath(p)}/${id}/diff?from=${from}&to=${to}`),
  restoreRecord: (p: SchemaPath, id: string, toVersion: number) =>
    api.post<VelocityRecord>(`${basePath(p)}/${id}/restore`, { to_version: toVersion }),

  // ---- audit ----------------------------------------------------------
  auditList: (q?: {
    actor?: string;
    schema?: string;
    entity?: string;
    operation?: string;
    outcome?: string;
    since?: string;
    limit?: number;
  }) => {
    const params = new URLSearchParams();
    Object.entries(q ?? {}).forEach(([k, v]) => {
      if (v !== undefined && v !== "") params.set(k, String(v));
    });
    const qs = params.toString();
    return api.get<ListResponse<AuditEntry>>(qs ? `/api/platform/audit?${qs}` : "/api/platform/audit");
  },
  auditVerify: () => api.get<AuditVerifyResponse>("/api/platform/audit/verify"),

  // ---- portal config --------------------------------------------------
  portalConfig: () =>
    api.get<PortalConfig>("/config.json").catch(() => ({}) as PortalConfig),
};
