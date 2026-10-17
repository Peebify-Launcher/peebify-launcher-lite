import { defineConfig, type Plugin } from "vite";
import { existsSync, mkdirSync, readFileSync, readdirSync, renameSync, rmSync } from "node:fs";
import { join, relative, resolve } from "node:path";
import react from "@vitejs/plugin-react";

const releaseVersion: string = JSON.parse(
  readFileSync(resolve(import.meta.dirname, "../package.json"), "utf-8"),
).version;
const sourcemapArchive = resolve(import.meta.dirname, "../dist/sourcemaps", releaseVersion);

function findSourcemaps(dir: string): string[] {
  if (!existsSync(dir)) return [];
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) return findSourcemaps(full);
    return entry.isFile() && entry.name.endsWith(".map") ? [full] : [];
  });
}

function archiveSourcemaps(): Plugin {
  return {
    name: "peebify-archive-sourcemaps",
    apply: "build",
    writeBundle(options) {
      const outDir = options.dir ?? resolve(import.meta.dirname, "dist");
      rmSync(sourcemapArchive, { recursive: true, force: true });
      for (const map of findSourcemaps(outDir)) {
        const target = join(sourcemapArchive, relative(outDir, map));
        mkdirSync(resolve(target, ".."), { recursive: true });
        renameSync(map, target);
      }
      const leftover = findSourcemaps(outDir);
      if (leftover.length > 0) {
        this.error(`source maps left inside ${outDir}: ${leftover.join(", ")}`);
      }
      this.info(`source maps archived to ${sourcemapArchive}`);
    },
  };
}

function hudStrings(): Plugin {
  return {
    name: "peebify-hud-strings",
    enforce: "pre",
    load(id) {
      const [file, query = ""] = id.split("?");
      if (!file.endsWith(".json") || !new URLSearchParams(query).has("hud")) return null;
      const { _meta, hud } = JSON.parse(readFileSync(file, "utf-8").replace(/^﻿/, ""));
      return { code: `export default ${JSON.stringify({ _meta, hud })};`, moduleType: "js" };
    },
  };
}

export default defineConfig({
  plugins: [react(), archiveSourcemaps(), hudStrings()],
  clearScreen: false,
  build: {
    sourcemap: "hidden",
    rollupOptions: {
      input: {
        main: resolve(import.meta.dirname, "index.html"),
        overlay: resolve(import.meta.dirname, "overlay.html"),
        hud: resolve(import.meta.dirname, "hud.html"),
      },
    },
  },
  server: {
    port: 1420,
    strictPort: true,
  },
});
