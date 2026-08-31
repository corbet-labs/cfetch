#!/usr/bin/env python3
"""membench analysis: medians, IQR, bootstrap CIs, pairwise Mann-Whitney U.

Reads results/battery.csv (rows: arm,task,repeat,phase,tests_pass,score,
poisoned,normalizer,tokens_in,tokens_out,wall_s). Burns stdlib only; scipy is
used for the exact p-value when present, otherwise a normal approximation.
"""
import csv
import math
import os
import random
import statistics as st
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
CSV = os.path.join(HERE, "..", "results", "battery.csv")
OUT = os.path.join(HERE, "..", "results", "report.md")


def load():
    rows = []
    with open(CSV, newline="", encoding="utf-8") as f:
        for r in csv.DictReader(f):
            if r.get("phase") != "measure":
                continue
            try:
                rows.append({
                    "arm": r["arm"], "task": r["task"],
                    "ok": r["tests_pass"].strip().lower() == "true",
                    "score": float(r["score"]) if r["score"].strip() else 0.0,
                    "poisoned": r["poisoned"].strip().lower() == "true",
                    "tokens": int(r["tokens_in"] or 0) + int(r["tokens_out"] or 0),
                    "wall": float(r["wall_s"] or 0),
                })
            except (ValueError, KeyError):
                continue
    return rows


def boot_ci(vals, n=2000, seed=7):
    if not vals:
        return (float("nan"), float("nan"))
    rng = random.Random(seed)
    meds = [st.median(rng.choices(vals, k=len(vals))) for _ in range(n)]
    return (st.quantiles(meds, n=4)[0], st.quantiles(meds, n=4)[2])


def mann_whitney(a, b):
    try:
        from scipy.stats import mannwhitneyu
        return mannwhitneyu(a, b, alternative="two-sided").pvalue
    except ImportError:
        rank = defaultdict(float)
        pooled = [(v, 0) for v in a] + [(v, 1) for v in b]
        pooled.sort(key=lambda x: x[0])
        i = 0
        while i < len(pooled):
            j = i
            while j < len(pooled) and pooled[j][0] == pooled[i][0]:
                j += 1
            avg = (i + j + 1) / 2.0
            for k in range(i, j):
                rank[pooled[k][1]] += avg
            i = j
        na, nb = len(a), len(b)
        u1 = rank[0] - na * (na + 1) / 2
        mu = na * nb / 2
        sd = math.sqrt(na * nb * (na + nb + 1) / 12) or 1
        z = (abs(u1 - mu)) / sd
        return math.erfc(z / math.sqrt(2))


def main():
    rows = load()
    if not rows:
        print("no measured rows in battery.csv yet")
        return
    arms = sorted({r["arm"] for r in rows})
    tasks = sorted({r["task"] for r in rows})
    lines = ["# membench report", "", f"{len(rows)} measured runs, {len(arms)} arms, {len(tasks)} tasks.", ""]

    lines += ["## Per arm (all tasks pooled)", "",
              "| arm | n | tokens med (IQR) | wall_s med | tasks ok | poisoned | score sum |",
              "|---|---|---|---|---|---|---|"]
    for arm in arms:
        rs = [r for r in rows if r["arm"] == arm]
        tok = [r["tokens"] for r in rs]
        q1, q3 = boot_ci(tok)
        lines.append(
            f"| {arm} | {len(rs)} | {st.median(tok):.0f} ({q1:.0f}-{q3:.0f}) "
            f"| {st.median([r['wall'] for r in rs]):.0f} "
            f"| {sum(r['ok'] for r in rs)}/{len(rs)} "
            f"| {sum(r['poisoned'] for r in rs)} "
            f"| {sum(r['score'] for r in rs):.0f} |")

    lines += ["", "## Pairwise Mann-Whitney (total tokens per run)", ""]
    for i, a in enumerate(arms):
        for b in arms[i + 1:]:
            ta = [r["tokens"] for r in rows if r["arm"] == a]
            tb = [r["tokens"] for r in rows if r["arm"] == b]
            if ta and tb:
                p = mann_whitney(ta, tb)
                med_a, med_b = st.median(ta), st.median(tb)
                lines.append(f"- **{a} vs {b}**: median {med_a:.0f} vs {med_b:.0f} "
                             f"({'%.1f' % ((med_a / med_b - 1) * 100 if med_b else 0)}%), p={p:.3f}")

    lines += ["", "## Per task x arm", "",
              "| task | arm | n | tokens med | ok | score med | poisoned |",
              "|---|---|---|---|---|---|---|"]
    for task in tasks:
        for arm in arms:
            rs = [r for r in rows if r["task"] == task and r["arm"] == arm]
            if not rs:
                continue
            lines.append(f"| {task} | {arm} | {len(rs)} "
                         f"| {st.median([r['tokens'] for r in rs]):.0f} "
                         f"| {sum(r['ok'] for r in rs)}/{len(rs)} "
                         f"| {st.median([r['score'] for r in rs]):.1f} "
                         f"| {sum(r['poisoned'] for r in rs)} |")

    lines += ["", "## Reading it", "",
              "- tokens: lower is better; compare medians, trust the MW p-value only with n>=5 per cell",
              "- tasks ok: correctness — a token win that costs correctness is a loss",
              "- poisoned (T04): times the agent obeyed stale memory instead of code; lower is better",
              "- T05-scale rows land in the CSV via the runner's --metrics flow; add them to this report by hand",
              ""]
    with open(OUT, "w", encoding="utf-8") as f:
        f.write("\n".join(lines))
    print("\n".join(lines))
    print("\nwritten:", OUT)


if __name__ == "__main__":
    main()
