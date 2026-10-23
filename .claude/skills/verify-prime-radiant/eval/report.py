#!/usr/bin/env python3
"""Summarizes score.py output. usage: report.py SCORES [SPLIT]"""
import collections, json, sys

rows = [json.loads(l) for l in open(sys.argv[1])]
split = sys.argv[2] if len(sys.argv) > 2 else None
rows = [r for r in rows if split in (None, r["split"])]


def line(label, group):
    n = len(group)
    strict = sum(1 for r in group if r["strict_rank"] and r["strict_rank"] <= 3)
    family = sum(1 for r in group if r["family_rank"] and r["family_rank"] <= 3)
    first = sum(1 for r in group if r["family_rank"] == 1)
    mrr = sum(1 / r["family_rank"] for r in group if r["family_rank"]) / n if n else 0
    print(f"{label:<28} n={n:<4} family@3 {family / n:.2f} ({family})  strict@3 {strict / n:.2f}  family@1 {first / n:.2f}  MRR@5 {mrr:.2f}")


print(f"split: {split or 'all'}  errors: {sum(1 for r in rows if r['error'])}")
line("all", rows)
by = collections.defaultdict(list)
for r in rows:
    by[r["source"]].append(r)
for source in sorted(by):
    line(source, by[source])
by = collections.defaultdict(list)
for r in rows:
    by[(r["machine"], r["source"])].append(r)
for key in sorted(by):
    line(" ".join(key), by[key])
