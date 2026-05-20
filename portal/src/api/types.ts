/**
 * Wire types for the velocity-api endpoints the portal consumes.
 * These mirror the JSON shapes documented in docs/design.md — kept loose
 * (Record<string, unknown>) where the API returns arbitrary user payloads.
 */

export interface RegistryIndex {
  service: string;
  ready: boolean;
  schemas: number;
  paths: string[];
}

export interface SchemaPath {
  org: string;
  app: string;
  domain: string;
  object: string;
  version: string;
}

export function parseSchemaPath(p: string): SchemaPath {
  const [org, app, domain, object, version] = p.split("/");
  return { org, app, domain, object, version };
}

export function joinSchemaPath(p: SchemaPath): string {
  return `${p.org}/${p.app}/${p.domain}/${p.object}/${p.version}`;
}

export interface VelocityRecord {
  id: string;
  version: number;
  created_at: string;
  updated_at: string;
  created_by?: string;
  updated_by?: string;
  deleted_at?: string | null;
  [field: string]: unknown;
}

export interface ListResponse<T = VelocityRecord> {
  items: T[];
  next_cursor?: string;
  total?: number;
}

export interface AuditEntry {
  id: number;
  timestamp: string;
  actor_id: string;
  actor_type?: string;
  schema_kind?: string;
  entity_id?: string;
  operation: string;
  outcome: string;
  fail_mode?: string;
  request_id?: string;
  hash_chain?: string;
  details?: Record<string, unknown>;
}

export interface AuditVerifyResponse {
  ok: boolean;
  first_break_id?: number;
  message?: string;
}

export interface HistoryEntry {
  version: number;
  timestamp: string;
  actor_id?: string;
  operation: string;
  diff?: unknown;
}

export interface Identity {
  actor_id: string;
  actor_type: string;
  attributes?: Record<string, string>;
}

/** Portal-side config served from /api/portal/config (or a static config.json). */
export interface PortalConfig {
  /**
   * Default AuthStrategy reference to redirect unauthenticated users to.
   * Format: "{namespace}/{name}". Nginx may substitute this at deploy time
   * via templated index.html or a /config.json file.
   */
  default_auth_strategy?: string;
  /** Optional URL of the central Grafana — link-outs from Metrics pages. */
  grafana_url?: string;
  /** Display banner (e.g. "PROD" / "STAGE") shown in the header. */
  environment?: string;
}
