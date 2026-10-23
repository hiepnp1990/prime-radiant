#!/usr/bin/env python3
"""Runs each eval query through find on a data folder and records where the expected session lands.
usage: score.py BIN DATA ITEMS [EXCLUDE...]
ITEMS is JSONL with item, split, source, machine, key, family, query. Prints one JSON line per item.
EXCLUDE keys (and keys under them, such as their subagents) are dropped from results: pass the
sessions that wrote the queries, since their transcripts quote every query."""
import json, os, subprocess, sys

BIN, DATA, ITEMS = sys.argv[1], os.path.expanduser(sys.argv[2]), sys.argv[3]
EXCLUDE = tuple(sys.argv[4:])


def rank(keys, wanted):
    return next((i + 1 for i, key in enumerate(keys) if key in wanted), None)


for line in open(ITEMS):
    item = json.loads(line)
    out = subprocess.run([BIN, "find", item["query"], "--data", DATA, "--json", "--limit", "8"],
                         capture_output=True, text=True)
    hits = json.loads(out.stdout) if out.returncode == 0 else []
    keys = [h["key"] for h in hits if not h["key"].startswith(EXCLUDE)][:5]
    print(json.dumps({
        "item": item["item"], "split": item["split"], "source": item["source"], "machine": item["machine"],
        "key": item["key"], "query": item["query"], "top": keys,
        "strict_rank": rank(keys, {item["key"]}),
        "family_rank": rank(keys, {item["key"], *item["family"]}),
        "error": out.stderr.strip()[:200] if out.returncode else None,
    }))
