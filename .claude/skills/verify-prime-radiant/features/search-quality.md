# Search quality

Measures how often a topic query finds the session the user means, on the user's own sessions. The ground truth comes from each harness's own files, not from Prime Radiant's index, so a bug in indexing can't hide itself. Fixture tests can't show this: ranking only shows its quality on a real corpus.

## Sub-features

- `truth` extracts each main session's native title, first typed request, and family. A family is the session a fork came from, its forks, and the harness session behind a T3 Code thread.
- `sample` draws items per machine and source. It leaves out sessions too short to describe and sessions whose first request matches another family's. It splits train and test by hash.
- `score` runs each query through `find` and records where the expected session and its family land.
- `report` prints recall@3 (strict and family), family@1, and MRR by source and machine.

## How to get to it (user POV)

- The user asks how good search is, or a change touches ranking, tokenizing, titles, or parsing.

## Driving it with verify.sh

Preconditions:

- The user has said yes to reading their real session folders, read-only. All outputs go in a private folder outside the repo (OUT below).
- A data folder holding those sessions: the user's hub data folder, or an `import` into a scratch folder.

- **Write the bar first.** Before scoring, write the pass bar and the protocol to `OUT/README.md`: which split decides, and the target recall. Ranking changes may look only at train-split misses. Score the test split once per ranking version.
- **Ground truth.** On each machine, run `python3 eval/ground_truth.py <machine-name> > OUT/truth-<machine-name>.jsonl`. Over SSH, run `ssh <host> python3 - <machine-name> < eval/ground_truth.py > OUT/truth-<machine-name>.jsonl`.
- **Sample.** Run `python3 eval/sample.py OUT/items.jsonl OUT/truth-*.jsonl --seed <n> --exclude <key of this session>`. It prints the counts left out and the items per machine, source, and split.
- **Queries.** Give fresh agents only each item's `item`, `title`, and `first_request`. Ask for the 4 to 12 word phrase the user would type weeks later, in their own words. At most 3 consecutive words copied, no IDs, links, or paths. Merge the phrases into each item as `query`.
- **Score.** Run `python3 eval/score.py <bin> <data dir> OUT/items-with-queries.jsonl <key of this session> > OUT/scores-<version>.jsonl`. Pass every session that saw the queries, since their transcripts quote them and would crowd the results.
- **Report.** Run `python3 eval/report.py OUT/scores-<version>.jsonl test`.

## Gotchas

- Harnesses inject text that looks like a first request: an app's worktree preamble, a plugin list, IDE context. `ground_truth.py` skips the ones known so far. Check the most repeated openings after each harness update.
- Many sessions repeat one prompt, such as scheduled or batch runs. They are left out, because a topic can't tell them apart.
- Queries written in strict paraphrase are harder than how many users type. Report the rule the queries followed with the number.
- Never put items, queries, scores, or titles in the repo. They are the user's sessions.
