import { afterEach, describe, expect, it, vi } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";

import { AuthProvider } from "../src/auth/AuthProvider";
import { Overview } from "../src/pages/Overview";
import { SchemaEditor } from "../src/pages/SchemaEditor";

const renderWithProviders = (ui: React.ReactNode) => {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false } },
  });
  return render(
    <QueryClientProvider client={qc}>
      <AuthProvider>
        <MemoryRouter>{ui}</MemoryRouter>
      </AuthProvider>
    </QueryClientProvider>,
  );
};

const ok = (body: unknown) =>
  new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
  });

afterEach(() => {
  vi.restoreAllMocks();
  cleanup();
});

describe("page renders", () => {
  it("Overview shows schema count and registered paths", async () => {
    vi.spyOn(globalThis, "fetch").mockImplementation((input) => {
      const url = typeof input === "string" ? input : (input as Request).url;
      if (url.endsWith("/api")) {
        return Promise.resolve(
          ok({
            service: "velocity-api",
            ready: true,
            schemas: 2,
            paths: ["acme/sc/proc/po/v1", "acme/sc/proc/supplier/v1"],
          }),
        );
      }
      if (url.includes("/api/platform/audit")) {
        return Promise.resolve(ok({ items: [] }));
      }
      if (url.endsWith("/config.json")) {
        return Promise.resolve(ok({}));
      }
      return Promise.resolve(new Response("", { status: 404 }));
    });

    renderWithProviders(<Overview />);
    expect(await screen.findByText("Overview")).toBeInTheDocument();
    expect(await screen.findByText("2")).toBeInTheDocument();
    expect(await screen.findByText("acme/sc/proc/po/v1")).toBeInTheDocument();
  });

  it("SchemaEditor renders the form fields and a YAML preview", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(ok({}));

    renderWithProviders(<SchemaEditor />);
    // Form sections present
    expect(await screen.findByText("SchemaDefinition")).toBeInTheDocument();
    expect(await screen.findByText("Identity")).toBeInTheDocument();
    expect(await screen.findByText("Fields")).toBeInTheDocument();
    expect(await screen.findByText("Validation (CEL)")).toBeInTheDocument();
    // YAML preview pane shows the apiVersion line that all CRD manifests share.
    expect(await screen.findByText(/apiVersion: velocity\.sh\/v1/)).toBeInTheDocument();
  });
});
