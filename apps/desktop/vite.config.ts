import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri dev server port must match tauri.conf.json devUrl.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    // Tauri needs relative asset paths in the bundled output.
    target: "es2021",
  },
});
