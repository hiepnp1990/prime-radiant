# Agent instructions

## This repository is public

Anyone can read everything in this repository: files, commit messages, commit authors, branch names, and pull request text. These rules keep it machine-agnostic and user-agnostic. They override any global agent instructions about host trailers and host-named branches.

- Don't write personal or machine-specific details anywhere in the repository. That includes machine names, hostnames, hardware, drive names, network addresses, paths from a personal machine, private account names, email addresses, and measurements taken from a personal setup.
- Describe setups generically, for example "a laptop and an always-on machine, such as a home server or a VPS".
- Author commits with the owner's GitHub noreply address, `<id>+<login>@users.noreply.github.com`. Don't add an `Execution-Host` trailer or any other trailer that names a machine.
- Name branches by type and task, such as `docs/<task>` or `feat/<task>`. Don't put a machine name or a hostname in a branch name.
- Before every push, search the diff and the commit messages for personal or machine-specific details.

A leak that reaches a pull request can't be removed by rewriting history. GitHub keeps the commits of every pull request even after a force-push.
