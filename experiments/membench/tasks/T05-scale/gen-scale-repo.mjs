
import { mkdirSync, writeFileSync } from "node:fs";
// Synthetic mono-repo: N modules with cross-imports + N markdown notes.
// Usage: node gen-scale-repo.mjs [modules] [outdir]
const n = Number(process.argv[2] ?? 300);
const dir = process.argv[3] ?? "scale-repo";
for (let i = 0; i < n; i++) {
  mkdirSync(dir + "/src/mod" + i, { recursive: true });
  mkdirSync(dir + "/notes", { recursive: true });
  const dep = i < n - 1 ? `import { m${i + 1} } from "../mod${i + 1}/index.js";` : "";
  writeFileSync(dir + "/src/mod" + i + "/index.js", `${dep}
export function m${i}() { return ${i}; }
// Module ${i}: handles the ${["billing", "routing", "packing", "audit", "sync"][i % 5]} concern for cluster ${Math.floor(i / 10)}.
`);
}
for (let i = 0; i < Math.floor(n / 3); i++) {
  writeFileSync(dir + "/notes/note" + i + ".md",
    "# Decision " + i + "\n\nWe chose approach " + (i % 4) + " for the " +
    ["cache", "queue", "index", "retry"][i % 4] + " because of latency budget " + (i % 7) + ".\n");
}
console.log("generated", n, "modules in", dir);
