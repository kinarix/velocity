/**
 * Client for the platform-api CRD read/write endpoints (Phase 12c).
 *
 * Auth is handled by the `velocity_session` cookie already set by the OIDC
 * flow — no Authorization header management needed here.
 */
import { api } from "./client";

// Minimal CRD envelope type — the full spec lives in the kind-specific CRD.
export interface KubeObject {
  apiVersion: string;
  kind: string;
  metadata: {
    name: string;
    namespace?: string;
    uid?: string;
    resourceVersion?: string;
    labels?: Record<string, string>;
    annotations?: Record<string, string>;
    creationTimestamp?: string;
    deletionTimestamp?: string;
  };
  spec?: unknown;
  status?: unknown;
}

export interface KubeList<T extends KubeObject = KubeObject> {
  apiVersion: string;
  kind: string;
  items: T[];
}

const BASE = "/api/platform/objects";

export function listObjects<T extends KubeObject = KubeObject>(
  kind: string,
  namespace?: string,
): Promise<KubeList<T>> {
  const url = namespace ? `${BASE}/${kind}?namespace=${encodeURIComponent(namespace)}` : `${BASE}/${kind}`;
  return api.get<KubeList<T>>(url);
}

export function getObject<T extends KubeObject = KubeObject>(
  kind: string,
  namespace: string,
  name: string,
): Promise<T> {
  return api.get<T>(`${BASE}/${kind}/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}`);
}

export function applyObject(
  kind: string,
  namespace: string,
  name: string,
  body: KubeObject,
): Promise<KubeObject> {
  return api.put<KubeObject>(
    `${BASE}/${kind}/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}`,
    body,
  );
}

export function deleteObject(kind: string, namespace: string, name: string): Promise<void> {
  return api.delete<void>(
    `${BASE}/${kind}/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}`,
  );
}
