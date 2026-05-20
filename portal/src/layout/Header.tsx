import { useQuery } from "@tanstack/react-query";

import { api } from "../api/client";
import { useAuth } from "../auth/useAuth";

interface Version {
  service: string;
  version: string;
  git_sha: string;
  ready: boolean;
}

export function Header() {
  const { config, logout } = useAuth();
  const { data: version } = useQuery({
    queryKey: ["version"],
    queryFn: () => api.get<Version>("/version"),
    refetchInterval: 30_000,
  });

  // Identity is intentionally not displayed: velocity-api currently has no
  // /api/whoami endpoint, so we can't tell the user who they are after the
  // OIDC callback. Showing "unauthenticated" here would mislead anyone who
  // *did* sign in. The sign-out button stays — it just hits /auth/logout.
  return (
    <header className="flex items-center justify-between h-12 px-4 border-b border-ink-700 bg-ink-900">
      <div className="flex items-center gap-3">
        <div className="text-amber-500 font-bold tracking-wider">VELOCITY</div>
        <span className="text-ink-400 text-xs">portal</span>
        {config.environment && (
          <span className="badge badge-warn">{config.environment}</span>
        )}
        {version && (
          <span className="badge" title={version.git_sha}>
            v{version.version}
          </span>
        )}
        {version && (
          <span className={`badge ${version.ready ? "badge-ok" : "badge-err"}`}>
            {version.ready ? "ready" : "not ready"}
          </span>
        )}
      </div>
      <div className="flex items-center gap-3 text-xs">
        <button className="btn btn-ghost" onClick={() => void logout()}>
          sign out
        </button>
      </div>
    </header>
  );
}
