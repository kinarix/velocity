import { useMemo, useState } from "react";

import { PageTitle } from "../components/PageTitle";
import { SplitEditor } from "../components/SplitEditor";

type Kind = "jwt" | "oidc" | "api_key" | "composite";

export function AuthStrategyEditor() {
  const [name, setName] = useState("default");
  const [namespace, setNamespace] = useState("velocity-system");
  const [kind, setKind] = useState<Kind>("oidc");
  const [issuer, setIssuer] = useState("https://issuer.example.com");
  const [clientId, setClientId] = useState("velocity-portal");
  const [redirectUri, setRedirectUri] = useState("https://portal.example.com/auth/callback");
  const [jwksUrl, setJwksUrl] = useState("");
  const [revocationFailOpen, setRevocationFailOpen] = useState(false);

  const config: Record<string, unknown> = useMemo(() => {
    const c: Record<string, unknown> = {};
    if (kind === "oidc") {
      c.oidc = {
        issuer,
        clientId,
        redirectUri,
        scopes: ["openid", "email", "profile"],
      };
    } else if (kind === "jwt") {
      c.jwt = { jwksUrl, audience: clientId };
    } else if (kind === "api_key") {
      c.apiKey = { header: "x-api-key" };
    }
    c.revocation = { failOpen: revocationFailOpen };
    return c;
  }, [kind, issuer, clientId, redirectUri, jwksUrl, revocationFailOpen]);

  const value = {
    apiVersion: "velocity.sh/v1",
    kind: "AuthStrategy",
    metadata: { name, namespace },
    spec: { kind, config },
  };

  return (
    <>
      <PageTitle
        title="New AuthStrategy"
        subtitle="Generate a CRD manifest. Apply with `velocity apply -f -`."
      />
      <SplitEditor
        filename={`${name}.authstrategy.yaml`}
        value={value}
        form={
          <div className="space-y-3 text-xs">
            <div>
              <label className="label">Name</label>
              <input className="input" value={name} onChange={(e) => setName(e.target.value)} />
            </div>
            <div>
              <label className="label">Namespace</label>
              <input className="input" value={namespace} onChange={(e) => setNamespace(e.target.value)} />
            </div>
            <div>
              <label className="label">Kind</label>
              <select className="input" value={kind} onChange={(e) => setKind(e.target.value as Kind)}>
                <option value="oidc">oidc</option>
                <option value="jwt">jwt</option>
                <option value="api_key">api_key</option>
                <option value="composite">composite</option>
              </select>
            </div>

            {kind === "oidc" && (
              <>
                <div>
                  <label className="label">Issuer</label>
                  <input className="input" value={issuer} onChange={(e) => setIssuer(e.target.value)} />
                </div>
                <div>
                  <label className="label">Client ID</label>
                  <input className="input" value={clientId} onChange={(e) => setClientId(e.target.value)} />
                </div>
                <div>
                  <label className="label">Redirect URI</label>
                  <input className="input" value={redirectUri} onChange={(e) => setRedirectUri(e.target.value)} />
                </div>
              </>
            )}
            {kind === "jwt" && (
              <>
                <div>
                  <label className="label">JWKS URL</label>
                  <input className="input" value={jwksUrl} onChange={(e) => setJwksUrl(e.target.value)} />
                </div>
                <div>
                  <label className="label">Audience</label>
                  <input className="input" value={clientId} onChange={(e) => setClientId(e.target.value)} />
                </div>
              </>
            )}

            <label className="flex items-center gap-2">
              <input
                type="checkbox"
                checked={revocationFailOpen}
                onChange={(e) => setRevocationFailOpen(e.target.checked)}
              />
              <span>
                <span className="text-ink-200">Revocation fail-open</span>
                <span className="text-ink-400"> (ADR-003: opt-in, default deny)</span>
              </span>
            </label>
          </div>
        }
      />
    </>
  );
}
