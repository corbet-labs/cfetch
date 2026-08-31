
Task: this task measures INFRASTRUCTURE, not the model.
1. Generate: node gen-scale-repo.mjs 300 seed-scale/
2. Install the arm (baseline: nothing; agentsmd: copy AGENTS.md; openwolf: openwolf init; cfetch: cfetch init)
3. Run the arm's index/scan once, then measure with the runner's --metrics flag:
   - wall time of first scan, wall time of second (cached) scan
   - disk usage of the tool's state dir (.wolf/ or index) after scan
   - p50/p95 of 20 recall/find queries: "which module handles audit for cluster 12",
     "what did Decision 4 decide and why"
Record into results CSV; no verdict.json for this task.
