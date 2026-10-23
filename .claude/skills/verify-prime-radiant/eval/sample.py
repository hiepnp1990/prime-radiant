#!/usr/bin/env python3
"""Samples eval items from ground truth files.
usage: sample.py OUT TRUTH... [--exclude KEY]... [--seed N] [--per-group N]
Leaves out excluded sessions, sessions whose first request is under 40 characters, and sessions whose
first request matches another family's. Splits train/test by SHA-1 of the key."""
import argparse, collections, hashlib, json, random, re

parser = argparse.ArgumentParser()
parser.add_argument("out")
parser.add_argument("truths", nargs="+")
parser.add_argument("--exclude", action="append", default=[], help="a session key to leave out, such as the session running the eval")
parser.add_argument("--seed", type=int, default=1)
parser.add_argument("--per-group", type=int, default=40)
args = parser.parse_args()
EXCLUDE, out, truths = tuple(args.exclude), args.out, args.truths
rows = {}
for path in truths:
    for line in open(path):
        row = json.loads(line)
        rows.setdefault(row["key"], row)
norm = lambda text: re.sub(r"[^a-z0-9]+", " ", text.lower()).strip()[:200]
cluster = {key: key for key in rows}
def root(key):
    while cluster.setdefault(key, key) != key:
        key = cluster[key]
    return key
for row in rows.values():
    for other in row["family"]:
        cluster[root(other)] = root(row["key"])
clusters = collections.defaultdict(set)
for row in rows.values():
    clusters[norm(row["first_request"])].add(root(row["key"]))
seen = {text: len(roots) for text, roots in clusters.items()}
pool = collections.defaultdict(list)
dropped = collections.Counter()
for row in rows.values():
    if row["key"] in EXCLUDE or any(k in EXCLUDE for k in row["family"]):
        dropped["this run"] += 1
    elif len(row["first_request"]) < 40:
        dropped["request under 40 chars"] += 1
    elif seen[norm(row["first_request"])] > 1:
        dropped["same first request as another session"] += 1
    else:
        pool[(row["machine"], row["source"])].append(row)
random.seed(args.seed)
items = []
for group in sorted(pool):
    chosen = sorted(pool[group], key=lambda r: r["key"])
    random.shuffle(chosen)
    for row in chosen[:args.per_group]:
        split = "test" if int(hashlib.sha1(row["key"].encode()).hexdigest(), 16) % 2 else "train"
        items.append({"item": f"i{len(items):03d}", "split": split, **row})
with open(out, "w") as f:
    for item in items:
        f.write(json.dumps(item) + "\n")
print("left out:", dict(dropped))
print("items:", dict(collections.Counter((i["machine"], i["source"], i["split"]) for i in items)))
