import { afterEach, describe, expect, it, vi } from "vitest";

import { api, isApiError } from "../src/api/client";
import { parseSchemaPath, joinSchemaPath } from "../src/api/types";

const ok = (body: unknown, init: ResponseInit = {}) =>
  new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json", "x-request-id": "req-1" },
    ...init,
  });

const err = (status: number, body: unknown) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", "x-request-id": "req-1" },
  });

describe("api client", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("GETs JSON and returns parsed body", async () => {
    const fetchSpy = vi.spyOn(globalThis, "fetch").mockResolvedValue(ok({ a: 1 }));
    const out = await api.get<{ a: number }>("/api");
    expect(out).toEqual({ a: 1 });
    expect(fetchSpy).toHaveBeenCalledWith(
      "/api",
      expect.objectContaining({ method: "GET", credentials: "include" }),
    );
  });

  it("POSTs JSON with content-type header", async () => {
    const fetchSpy = vi.spyOn(globalThis, "fetch").mockResolvedValue(ok({ id: "x" }));
    await api.post("/api/a/b/c/d/v1", { foo: "bar" });
    const init = fetchSpy.mock.calls[0][1] as RequestInit;
    expect(init.method).toBe("POST");
    expect(init.body).toBe(JSON.stringify({ foo: "bar" }));
    expect((init.headers as Headers).get("content-type")).toBe("application/json");
  });

  it("throws ApiError on non-2xx and includes request_id", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      err(403, { code: "FORBIDDEN", message: "nope" }),
    );
    try {
      await api.get("/api/x/y/z/o/v1");
      throw new Error("should have thrown");
    } catch (e) {
      expect(isApiError(e)).toBe(true);
      if (isApiError(e)) {
        expect(e.status).toBe(403);
        expect(e.code).toBe("FORBIDDEN");
        expect(e.request_id).toBe("req-1");
      }
    }
  });

  it("dispatches velocity:unauthenticated on 401", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(err(401, { message: "no" }));
    const listener = vi.fn();
    window.addEventListener("velocity:unauthenticated", listener);
    try {
      await api.get("/api");
    } catch {
      /* expected */
    }
    expect(listener).toHaveBeenCalled();
    window.removeEventListener("velocity:unauthenticated", listener);
  });

  it("returns undefined for 204", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response(null, { status: 204 }));
    const out = await api.delete("/api/x/y/z/o/v1/123");
    expect(out).toBeUndefined();
  });
});

describe("schema path helpers", () => {
  it("round-trips org/app/domain/object/version", () => {
    const p = parseSchemaPath("acme/supply-chain/procurement/purchase-order/v1");
    expect(p).toEqual({
      org: "acme",
      app: "supply-chain",
      domain: "procurement",
      object: "purchase-order",
      version: "v1",
    });
    expect(joinSchemaPath(p)).toBe("acme/supply-chain/procurement/purchase-order/v1");
  });
});
