import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Same-origin in production: nginx serves the SPA and proxies /api and /auth
// to velocity-api on the same host. In dev, vite's proxy targets the dev API
// (defaults to http://127.0.0.1:8080 — override with VELOCITY_API_BASE).
const apiBase = process.env.VELOCITY_API_BASE ?? "http://127.0.0.1:8080";

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/api":      { target: apiBase, changeOrigin: true },
      "/auth":     { target: apiBase, changeOrigin: true },
      "/version":  { target: apiBase, changeOrigin: true },
      "/healthz":  { target: apiBase, changeOrigin: true },
      "/readyz":   { target: apiBase, changeOrigin: true },
    },
  },
});
