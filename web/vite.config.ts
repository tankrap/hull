import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Proxy the Hull API + SSE feed to the local hull-server during dev.
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5930,
    proxy: {
      "/api": { target: "http://127.0.0.1:8930", changeOrigin: true },
      "/health": { target: "http://127.0.0.1:8930", changeOrigin: true },
    },
  },
});
