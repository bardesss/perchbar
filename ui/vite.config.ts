import { defineConfig } from "vitest/config";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// Built into ui/dist, which the Rust build embeds. Tauri serves it from its
// own protocol, so asset paths stay relative.
export default defineConfig({
  plugins: [svelte()],
  base: "./",
  clearScreen: false,
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2022",
  },
  test: {
    include: ["src/**/*.test.ts"],
  },
});
