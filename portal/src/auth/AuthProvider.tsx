import { createContext, useCallback, useEffect, useMemo, useState } from "react";
import type { ReactNode } from "react";

import { endpoints } from "../api/endpoints";
import type { PortalConfig } from "../api/types";

interface AuthContextValue {
  config: PortalConfig;
  login: () => void;
  logout: () => Promise<void>;
}

// eslint-disable-next-line react-refresh/only-export-components
export const AuthContext = createContext<AuthContextValue | null>(null);

/**
 * Auth model:
 *   - velocity-api owns the OIDC redirect flow at /auth/login/{ns}/{name} →
 *     IdP → /auth/callback → session cookie on the same origin.
 *   - Portal never sees tokens. We dispatch a `velocity:unauthenticated`
 *     event from the API client on any 401; the provider catches it and
 *     bounces the browser to /auth/login.
 *   - Identity (actor_id, attributes) is NOT exposed in the UI today —
 *     there is no /api/whoami endpoint on velocity-api yet. Once that
 *     lands, swap a useQuery here and surface the user in the header.
 */
export function AuthProvider({ children }: { children: ReactNode }) {
  const [config, setConfig] = useState<PortalConfig>({});

  useEffect(() => {
    void endpoints.portalConfig().then(setConfig);
  }, []);

  const login = useCallback(() => {
    const ref = config.default_auth_strategy;
    if (!ref) {
      // Without a configured strategy there's no sensible target — surface it.
      console.warn(
        "velocity-portal: no default_auth_strategy configured; cannot start login flow.",
      );
      return;
    }
    const [ns, name] = ref.split("/");
    const url = new URL(`/auth/login/${ns}/${name}`, window.location.origin);
    url.searchParams.set("return_to", window.location.pathname + window.location.search);
    window.location.assign(url.toString());
  }, [config.default_auth_strategy]);

  const logout = useCallback(async () => {
    try {
      await fetch("/auth/logout", { method: "POST", credentials: "include" });
    } finally {
      window.location.assign("/");
    }
  }, []);

  useEffect(() => {
    const onUnauth = () => {
      if (config.default_auth_strategy) login();
    };
    window.addEventListener("velocity:unauthenticated", onUnauth);
    return () => window.removeEventListener("velocity:unauthenticated", onUnauth);
  }, [config.default_auth_strategy, login]);

  const value = useMemo<AuthContextValue>(
    () => ({ config, login, logout }),
    [config, login, logout],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}
