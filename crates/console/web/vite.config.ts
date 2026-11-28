// The telemetry page, built into `dist/` and embedded in the console (crates/console/src/lib.rs),
// which serves it at /telemetry and its files under /app/. The console's CSP allows only
// itself, so nothing may be inline: no `data:` URL in the CSS (written out as files here)
// and no asset inlined into the script.

import { createHash } from "node:crypto";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import type { Plugin } from "vite";
import { defineConfig } from "vitest/config";

const EXT: Record<string, string> = { "image/svg+xml": "svg", "font/woff2": "woff2", "font/woff": "woff", "image/png": "png" };

/** Each `url(data:...)` in the built CSS becomes a file beside it. */
function noDataUrls(): Plugin {
  return {
    name: "no-data-urls",
    enforce: "post",
    generateBundle(_, bundle) {
      for (const f of Object.values(bundle)) {
        if (f.type !== "asset" || !f.fileName.endsWith(".css")) continue;
        const css = String(f.source);
        f.source = css.replace(/url\((['"]?)data:([^;,]+)((?:;[^;,]+)*),(.*?)\1\)/g, (_m, _q, mime, params, payload) => {
          const ext = EXT[mime];
          if (!ext) this.error(`a data: URL of type ${mime} in ${f.fileName}`);
          const raw = params.includes(";base64") ? Buffer.from(payload, "base64") : Buffer.from(decodeURIComponent(payload));
          const name = `assets/data-${createHash("sha256").update(raw).digest("hex").slice(0, 12)}.${ext}`;
          if (!bundle[name]) this.emitFile({ type: "asset", fileName: name, source: raw });
          return `url(${name.slice("assets/".length)})`;
        });
        if (String(f.source).includes("data:")) this.error(`${f.fileName} still has a data: URL`);
      }
    },
  };
}

export default defineConfig({
  base: "/app/",
  plugins: [react(), tailwindcss(), noDataUrls()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    assetsInlineLimit: 0,
    sourcemap: false,
    // AG Grid is most of it; the page loads once per sign-in and is cached for good.
    chunkSizeWarningLimit: 1500,
    modulePreload: { polyfill: false },
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
