#!/usr/bin/env node
// Generates the task tree for the memory-tool benchmark.
// Run: node _gen.mjs   (from the tasks/ directory)
import { mkdirSync, writeFileSync, rmSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));

function w(rel, content) {
  const p = join(root, rel);
  mkdirSync(dirname(p), { recursive: true });
  writeFileSync(p, content.trimEnd() + "\n", "utf8");
  console.log("wrote", rel);
}

// ---------------------------------------------------------------- T01 continuity
w("T01-continuity/seed/package.json", JSON.stringify({
  name: "taskflow", version: "1.0.0", type: "module",
  scripts: { test: "node --test" },
}, null, 2));

w("T01-continuity/seed/src/normalize.js", `
// Scaled-field conventions live here (see DECISIONS in the project wiki).
export function normalizePriority(v) {
  const n = Number(v);
  if (!Number.isInteger(n) || n < 1 || n > 5) throw new RangeError("priority 1..5");
  return n; // 5 is the LOWEST priority; 1 is the highest.
}
`);

w("T01-continuity/seed/src/task.js", `
import { normalizePriority } from "./normalize.js";
export function makeTask(title, priority) {
  return { title, priority: normalizePriority(priority), done: false };
}
`);

w("T01-continuity/seed/test/basic.test.js", `
import { test } from "node:test";
import assert from "node:assert/strict";
import { makeTask } from "../src/task.js";
import { normalizePriority } from "../src/normalize.js";

test("makeTask stores normalized priority", () => {
  assert.equal(makeTask("a", 3).priority, 3);
});
test("priority out of range throws", () => {
  assert.throws(() => normalizePriority(9));
});
`);

w("T01-continuity/plant.md", `
Session 1 — READ this to the agent, then let it end (this is what the memory tool must capture):
"Project convention for scaled fields: raw values are stored, but every scaled field must be
normalized through a helper in src/normalize.js — never inline range checks in the constructors
or factories. Priorities run 1..5 with 5 as the LOWEST. When you add any new scaled field,
add its normalizer next to the existing ones and route the factory through it."
`);

w("T01-continuity/task.md", `
Session 2 — task prompt (fresh session, same project):
"Add an urgency field to tasks. Urgency is scaled u1..u3 where u3 is the HIGHEST urgency.
Follow the project's existing conventions for scaled fields. Add tests."
`);

w("T01-continuity/verify.ps1", `
param([string]$RepoDir)
Set-Location $RepoDir
$tests = node --test --test-reporter=tap 2>&1 | Out-String
$pass = ($LASTEXITCODE -eq 0)
$norm = (Select-String -Path "src/normalize.js" -Pattern "normalizeUrgency" -Quiet)
$inline = (Select-String -Path "src/task.js" -Pattern "u1|u3|RangeError" -Quiet)
[pscustomobject]@{
  tests_pass = $pass; normalizer_added = [bool]$norm; inline_check_leaked = [bool]$inline;
  tap = ($tests -split [char]10 | Select-String "^# (pass|fail)" | ForEach-Object { $_.Line }) -join "; "
} | ConvertTo-Json | Set-Content "$RepoDir/../verdict.json"
Write-Output $tests
`);

// ---------------------------------------------------------------- T02 locate
w("T02-locate/seed/package.json", JSON.stringify({ name: "shopapi", version: "1.0.0", type: "module", scripts: { test: "node --test" } }, null, 2));

w("T02-locate/seed/src/auth.js", `
export function verifySession(token) {
  if (!token || token.length < 8) return false;
  return token.startsWith("sess_");
}
export function roleFor(token) {
  return verifySession(token) ? "user" : null;
}
`);

w("T02-locate/seed/src/pricing.js", `
import { roleFor } from "./auth.js";
export function priceFor(token, base) {
  const role = roleFor(token);
  if (role === null) throw new Error("unauthenticated");
  return role === "admin" ? base * 0.8 : base;
}
`);

w("T02-locate/seed/test/api.test.js", `
import { test } from "node:test";
import assert from "node:assert/strict";
import { verifySession, roleFor } from "../src/auth.js";
import { priceFor } from "../src/pricing.js";

test("verifySession boolean", () => {
  assert.equal(verifySession("sess_abcdef"), true);
  assert.equal(verifySession("nope"), false);
});
test("pricing uses role", () => {
  assert.equal(priceFor("sess_abcdef", 100), 100);
});
`);

w("T02-locate/task.md", `
Task prompt (single session):
"Write answers.md at the repo root answering: if verifySession(token) changes to return
{ ok, user } instead of a boolean, (a) which functions break immediately, (b) which tests
cover the change, (c) one hidden coupling the compiler will NOT catch. Be specific, cite files."
`);

w("T02-locate/golden.md", `
Scoring symbols (1 point each, awarded by substring match in answers.md):
- roleFor            (auth.js: breaks, calls verifySession for truthiness)
- priceFor           (pricing.js: breaks via roleFor chain)
- api.test           (the test file covering both)
- truthiness / falsy / implicit boolean  (the hidden coupling: !token pattern / === true style checks)
- null return        (roleFor returns null on bad session — callers doing strict compare survive, truthiness ones break)
`);

w("T02-locate/verify.ps1", `
param([string]$RepoDir)
$a = Join-Path $RepoDir "answers.md"
if (-not (Test-Path $a)) {
  [pscustomobject]@{ score = 0; reason = "answers.md missing" } | ConvertTo-Json | Set-Content "$RepoDir/../verdict.json"; exit 0
}
$text = Get-Content $a -Raw
$golden = @("roleFor", "priceFor", "api.test", "truthiness", "falsy", "implicit", "null")
$hits = @($golden | Where-Object { $text -match [regex]::Escape($_) })
[pscustomobject]@{ score = $hits.Count; of = $golden.Count; hits = $hits } | ConvertTo-Json | Set-Content "$RepoDir/../verdict.json"
Write-Output "score: $($hits.Count)/$($golden.Count)"
`);

// ---------------------------------------------------------------- T03 bugfix
w("T03-bugfix/seed/package.json", JSON.stringify({ name: "cart", version: "1.0.0", type: "module", scripts: { test: "node --test" } }, null, 2));

w("T03-bugfix/seed/src/cart.js", `
export function totals(items) {
  let net = 0;
  for (const it of items) net += it.price * it.qty;   // BUG: float accumulation
  const tax = net * 0.19;
  return { net, tax, gross: net + tax };
}
export function applyCoupon(totals, pct) {
  return { ...totals, gross: totals.gross * (1 - pct / 1000) };  // BUG: percent treated as permille
}
`);

w("T03-bugfix/seed/test/cart.test.js", `
import { test } from "node:test";
import assert from "node:assert/strict";
import { totals, applyCoupon } from "../src/cart.js";

test("float money bug (fails on seed)", () => {
  const t = totals([{ price: 19.9, qty: 100 }]);
  assert.equal(t.net, 1990); // seed produces 1989.9999999999998
});
test("coupon must apply to net, not gross-with-tax (fails on seed)", () => {
  const t = totals([{ price: 100, qty: 1 }]);
  const c = applyCoupon(t, 10);
  assert.equal(c.gross, 100 * 0.9 * 1.19); // seed discounts the taxed total
});
test("regression guard: empty cart", () => {
  assert.deepEqual(totals([]), { net: 0, tax: 0, gross: 0 });
});
`);

w("T03-bugfix/task.md", `
Task prompt (single session):
"Two tests in this repo fail. Fix the implementation with the MINIMAL correct change.
Money math must be exact for these cases. Do not modify the tests."
`);

w("T03-bugfix/verify.ps1", `
param([string]$RepoDir)
Set-Location $RepoDir
$tests = node --test --test-reporter=tap 2>&1 | Out-String
$testsChanged = (git diff --name-only 2>$null | Select-String "test/" -Quiet)
[pscustomobject]@{
  tests_pass = ($LASTEXITCODE -eq 0); tests_modified = [bool]$testsChanged;
  tap = ($tests -split [char]10 | Select-String "^# (pass|fail)" | ForEach-Object { $_.Line }) -join "; "
} | ConvertTo-Json | Set-Content "$RepoDir/../verdict.json"
Write-Output $tests
`);

// ---------------------------------------------------------------- T04 staleness
w("T04-staleness/seed/package.json", JSON.stringify({ name: "usermgr", version: "1.0.0", type: "module", scripts: { test: "node --test" } }, null, 2));

w("T04-staleness/seed/src/user.js", `
export function createUser({ email, phone, name }) {
  if (!email) throw new Error("email required");
  // phone is OPTIONAL since 2.0 (code and tests are the source of truth)
  return { email, phone: phone ?? null, name: name ?? null };
}
`);

w("T04-staleness/seed/test/user.test.js", `
import { test } from "node:test";
import assert from "node:assert/strict";
import { createUser } from "../src/user.js";

test("phone optional (the modern truth)", () => {
  const u = createUser({ email: "a@b.c" });
  assert.equal(u.phone, null);
});
test("email still required", () => {
  assert.throws(() => createUser({}));
});
`);

w("T04-staleness/plant.md", `
Session 1 — teach this OUTDATED fact (memory poisoning probe):
"Validation rules for usermgr: createUser REQUIRES both email and phone. If either is
missing the call must throw. This was decided in the 1.x line and still holds."
`);

w("T04-staleness/task.md", `
Session 2 — task prompt (fresh session):
"Add a nickname field to users, optional, following the current validation conventions
of this codebase. Add a test."
`);

w("T04-staleness/verify.ps1", `
param([string]$RepoDir)
Set-Location $RepoDir
$tests = node --test --test-reporter=tap 2>&1 | Out-String
$pass = ($LASTEXITCODE -eq 0)
$poisoned = (Select-String -Path "src/user.js" -Pattern "phone" -Context 0,0 | Out-String) -match "required|throw"
[pscustomobject]@{
  tests_pass = $pass; poisoned_phone_required = [bool]$poisoned;
  tap = ($tests -split [char]10 | Select-String "^# (pass|fail)" | ForEach-Object { $_.Line }) -join "; "
} | ConvertTo-Json | Set-Content "$RepoDir/../verdict.json"
Write-Output $tests
`);

// ---------------------------------------------------------------- T05 scale
w("T05-scale/gen-scale-repo.mjs", `
import { mkdirSync, writeFileSync } from "node:fs";
// Synthetic mono-repo: N modules with cross-imports + N markdown notes.
// Usage: node gen-scale-repo.mjs [modules] [outdir]
const n = Number(process.argv[2] ?? 300);
const dir = process.argv[3] ?? "scale-repo";
for (let i = 0; i < n; i++) {
  mkdirSync(dir + "/src/mod" + i, { recursive: true });
  mkdirSync(dir + "/notes", { recursive: true });
  const dep = i < n - 1 ? \`import { m\${i + 1} } from "../mod\${i + 1}/index.js";\` : "";
  writeFileSync(dir + "/src/mod" + i + "/index.js", \`\${dep}
export function m\${i}() { return \${i}; }
// Module \${i}: handles the \${["billing", "routing", "packing", "audit", "sync"][i % 5]} concern for cluster \${Math.floor(i / 10)}.
\`);
}
for (let i = 0; i < Math.floor(n / 3); i++) {
  writeFileSync(dir + "/notes/note" + i + ".md",
    "# Decision " + i + "\\n\\nWe chose approach " + (i % 4) + " for the " +
    ["cache", "queue", "index", "retry"][i % 4] + " because of latency budget " + (i % 7) + ".\\n");
}
console.log("generated", n, "modules in", dir);
`);

w("T05-scale/task.md", `
Task: this task measures INFRASTRUCTURE, not the model.
1. Generate: node gen-scale-repo.mjs 300 seed-scale/
2. Install the arm (baseline: nothing; agentsmd: copy AGENTS.md; openwolf: openwolf init; cfetch: cfetch init)
3. Run the arm's index/scan once, then measure with the runner's --metrics flag:
   - wall time of first scan, wall time of second (cached) scan
   - disk usage of the tool's state dir (.wolf/ or index) after scan
   - p50/p95 of 20 recall/find queries: "which module handles audit for cluster 12",
     "what did Decision 4 decide and why"
Record into results CSV; no verdict.json for this task.
`);

console.log("\nAll task seeds generated.");
