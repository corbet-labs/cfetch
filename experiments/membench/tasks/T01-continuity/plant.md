
Session 1 — READ this to the agent, then let it end (this is what the memory tool must capture):
"Project convention for scaled fields: raw values are stored, but every scaled field must be
normalized through a helper in src/normalize.js — never inline range checks in the constructors
or factories. Priorities run 1..5 with 5 as the LOWEST. When you add any new scaled field,
add its normalizer next to the existing ones and route the factory through it."
