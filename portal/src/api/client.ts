/**
 * Thin fetch wrapper for the portal.
 *
 * Same-origin in production (nginx serves the SPA and proxies /api → velocity-api).
 * In dev, vite proxies the same paths. Either way, fetch with relative URLs.
 *
 * Authentication: the velocity-api OIDC flow lands a session cookie on the
 * same origin. We don't manage tokens here; on 401 we redirect the user to
 * /auth/login/{namespace}/{name} via the auth hook. The strategy reference
 * is pulled from /api/portal/config (a small JSON document the nginx layer
 * can substitute at deploy time) so the portal binary stays generic.
 */

export interface ApiError {
  status: number;
  code?: string;
  message: string;
  request_id?: string;
}

function isApiErrorBody(b: unknown): b is { code?: string; message?: string; request_id?: string } {
  return typeof b === "object" && b !== null;
}

async function request<T>(
  method: string,
  path: string,
  body?: unknown,
  init?: RequestInit,
): Promise<T> {
  const headers = new Headers(init?.headers);
  if (body !== undefined && !headers.has("Content-Type")) {
    headers.set("Content-Type", "application/json");
  }
  if (!headers.has("Accept")) {
    headers.set("Accept", "application/json");
  }

  const res = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    credentials: "include",
    ...init,
  });

  if (res.status === 401) {
    // Hand off to the auth flow. The hook subscribes to this event.
    window.dispatchEvent(new CustomEvent("velocity:unauthenticated"));
    throw <ApiError>{ status: 401, message: "unauthenticated" };
  }

  const requestId = res.headers.get("x-request-id") ?? undefined;
  const contentType = res.headers.get("content-type") ?? "";

  if (!res.ok) {
    let errBody: unknown = undefined;
    try {
      errBody = contentType.includes("json") ? await res.json() : await res.text();
    } catch {
      // swallow
    }
    const code = isApiErrorBody(errBody) ? errBody.code : undefined;
    const message =
      (isApiErrorBody(errBody) && errBody.message) ||
      (typeof errBody === "string" ? errBody : "") ||
      res.statusText ||
      `HTTP ${res.status}`;
    const apiErr: ApiError = { status: res.status, code, message, request_id: requestId };
    throw apiErr;
  }

  if (res.status === 204 || res.headers.get("content-length") === "0") {
    return undefined as T;
  }
  return (contentType.includes("json") ? res.json() : res.text()) as Promise<T>;
}

export const api = {
  get:    <T>(p: string, init?: RequestInit) => request<T>("GET",    p, undefined, init),
  post:   <T>(p: string, body?: unknown)     => request<T>("POST",   p, body),
  put:    <T>(p: string, body?: unknown)     => request<T>("PUT",    p, body),
  patch:  <T>(p: string, body?: unknown)     => request<T>("PATCH",  p, body),
  delete: <T>(p: string)                     => request<T>("DELETE", p),
};

export function isApiError(e: unknown): e is ApiError {
  return typeof e === "object" && e !== null && "status" in e && "message" in e;
}
