import { describe, expect, it } from "vitest";
import YAML from "yaml";

/**
 * The visual editors assemble JS objects and YAML-serialize them with
 * the same library the runtime uses. These tests pin the shape of the
 * generated CRDs so a future refactor in SchemaEditor.tsx doesn't quietly
 * change the apply-able manifest.
 */
describe("yaml manifest shape", () => {
  it("SchemaDefinition serializes apiVersion + kind + spec", () => {
    const value = {
      apiVersion: "velocity.sh/v1",
      kind: "SchemaDefinition",
      metadata: { name: "po", namespace: "acme-sc-proc" },
      spec: { object: "po", version: "v1", fields: [{ name: "id", kind: "uuid" }] },
    };
    const out = YAML.stringify(value);
    expect(out).toContain("apiVersion: velocity.sh/v1");
    expect(out).toContain("kind: SchemaDefinition");
    expect(out).toContain("name: po");
    expect(out).toContain("- name: id");
  });

  it("AuthStrategy omits unset config blocks", () => {
    const value = {
      apiVersion: "velocity.sh/v1",
      kind: "AuthStrategy",
      metadata: { name: "d", namespace: "ns" },
      spec: { kind: "jwt", config: { jwt: { jwksUrl: "https://x", audience: "v" } } },
    };
    const out = YAML.stringify(value);
    expect(out).not.toContain("oidc:");
    expect(out).toContain("jwksUrl: https://x");
  });
});
