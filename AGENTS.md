# Agent instructions

## This repository is public

Anyone can read everything in this repository: files, commit messages, commit authors, branch names, and pull request text. These rules keep it machine-agnostic and user-agnostic. They override any global agent instructions about host trailers and host-named branches.

- Don't write personal or machine-specific details anywhere in the repository. That includes machine names, hostnames, hardware, drive names, network addresses, paths from a personal machine, private account names, email addresses, and measurements taken from a personal setup.
- Describe setups generically, for example "a laptop and an always-on machine, such as a home server or a VPS".
- Author commits with the owner's GitHub noreply address, `<id>+<login>@users.noreply.github.com`. Don't add an `Execution-Host` trailer or any other trailer that names a machine.
- Name branches by type and task, such as `docs/<task>` or `feat/<task>`. Don't put a machine name or a hostname in a branch name.
- Before every push, search the diff and the commit messages for personal or machine-specific details.
- Merge a pull request by fast-forwarding `main` to a single commit that uses the noreply address. If the branch has more than one commit, squash it locally first. Don't merge with GitHub's merge buttons or with `gh pr merge`. Both write the account's primary email into the merge commit, and GitHub rejects the noreply address as the merge author.

A leak that reaches a pull request can't be removed by rewriting history. GitHub keeps the commits of every pull request even after a force-push.

## Verifying changes

Before you claim a change to collection, indexing, search, the hub, or the CLI works, drive it with `.claude/skills/verify-prime-radiant/` (read its `SKILL.md`). It builds this checkout, runs a disposable hub on copies of `tests/fixtures/`, and keeps the evidence of each command. Its search-quality eval runs on a user's own sessions only with their go-ahead, and its outputs never go into the repository.
