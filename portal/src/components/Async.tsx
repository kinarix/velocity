import type { ReactNode } from "react";
import { isApiError } from "../api/client";

/** Render-prop helper for react-query results — spinner / error / content. */
export function Async<T>({
  query,
  children,
  empty,
}: {
  query: { data?: T; isPending: boolean; error: unknown };
  children: (data: T) => ReactNode;
  empty?: ReactNode;
}) {
  if (query.isPending) {
    return <div className="text-xs text-ink-400 p-4">loading…</div>;
  }
  if (query.error) {
    const e = query.error;
    const msg = isApiError(e) ? `${e.status} ${e.message}` : String(e);
    return <div className="text-xs text-red-300 p-4">error: {msg}</div>;
  }
  if (query.data === undefined) return <>{empty ?? null}</>;
  return <>{children(query.data)}</>;
}
