import adapter from "@sveltejs/adapter-static";
import { sveltekit } from "@sveltejs/kit/vite";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [
    sveltekit({
      compilerOptions: {
        runes: ({ filename }) =>
          filename.split(/[/\\]/).includes("node_modules")
            ? undefined
            : true,
      },

      adapter: adapter({
        fallback: "index.html",
      }),
    }),
  ],

  clearScreen: false,

  server: {
    host: "localhost",
    port: 1420,
    strictPort: true,

    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },

  test: {
    environment: "jsdom",
    environmentOptions: { jsdom: { pretendToBeVisual: true } },
    setupFiles: ["./src/test/setup.ts"],
    restoreMocks: true,
    clearMocks: true,
  },
  resolve: { conditions: ["browser"] },
});
