#!/usr/bin/env python3
"""Ground truth for search, read straight from each harness's own files, not through Prime Radiant.

usage: ground_truth.py MACHINE [HOME]
Prints one JSON line per main session found under HOME: its source, native ID, expected session key,
native title, first typed request, and family (keys of sessions that share its history: the session
it was forked from, forks of it, and the harness session behind a T3 Code thread)."""
import glob, json, os, re, sqlite3, subprocess, sys

MACHINE = sys.argv[1]
HOME = os.path.expanduser(sys.argv[2] if len(sys.argv) > 2 else "~")
UUID = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
ROLLOUT = re.compile(rf"^rollout-.*?({UUID.pattern})\.jsonl(\.zst)?$")
TAGS = re.compile(r"<[^>]+>")
SKIP = ("# AGENTS.md", "<environment_context>", "<user_instructions>", "<permissions", "<system-reminder>",
        "Here is a list of plugins", "<codex_delegation>")
PREAMBLES = ("You are operating in a git worktree", "Here is a list of plugins")
IDE_REQUEST = "## My request"


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return " ".join(p.get("text", "") for p in content if isinstance(p, dict) and p.get("type") in ("text", "input_text")
                        and not injected(p.get("text", "")))
    return ""


def injected(text):
    return re.sub(r"\s+", " ", TAGS.sub(" ", text)).strip().startswith(PREAMBLES)


def clean(text):
    if IDE_REQUEST in text:
        text = text[text.index(IDE_REQUEST):].split("\n", 1)[-1]
    return re.sub(r"\s+", " ", TAGS.sub(" ", text)).strip()[:900]


def emit(source, native_id, title, request, family=()):
    print(json.dumps({"source": source, "machine": MACHINE, "native_id": native_id,
                      "key": f"{source}/{native_id}", "title": (title or "").strip()[:200],
                      "first_request": clean(request or ""), "family": sorted(set(family))}))


def claude_code():
    root = f"{HOME}/.claude/projects"
    forks = {}
    rows = []
    for path in glob.glob(f"{root}/*/*.jsonl"):
        folder, name = path.split("/")[-2:]
        if name.startswith("agent-") or folder.startswith("ssh-"):
            continue
        sid = name[:-6]
        title = custom = request = origin = None
        try:
            lines = open(path, encoding="utf-8", errors="replace")
        except OSError:
            continue
        for line in lines:
            try:
                o = json.loads(line)
            except json.JSONDecodeError:
                continue
            other = o.get("sessionId")
            if other and other != sid and UUID.fullmatch(other) and origin is None:
                origin = other
            if o.get("customTitle"):
                custom = o["customTitle"]
            if o.get("aiTitle"):
                title = o["aiTitle"]
            if (request is None and o.get("type") == "user" and not o.get("isMeta")
                    and other == sid and o.get("message", {}).get("role") == "user"):
                text = text_of(o["message"].get("content"))
                if text.strip() and not text.lstrip().startswith(("<local-command", "<command-name>/clear", "Caveat:")):
                    request = text
        meta = f"{root}/{folder}/{sid}/custom-title.json"
        if os.path.exists(meta):
            try:
                custom = json.load(open(meta)).get("customTitle") or custom
            except (OSError, ValueError):
                pass
        if origin:
            forks.setdefault(origin, set()).add(sid)
        rows.append((sid, custom or title, request, origin))
    for sid, title, request, origin in rows:
        family = {f"claude-code/{x}" for x in forks.get(sid, ())}
        if origin:
            family |= {f"claude-code/{origin}"} | {f"claude-code/{x}" for x in forks.get(origin, ()) if x != sid}
        emit("claude-code", sid, title, request, family)


def codex():
    root = f"{HOME}/.codex"
    names = {}
    if os.path.exists(f"{root}/session_index.jsonl"):
        for line in open(f"{root}/session_index.jsonl"):
            try:
                o = json.loads(line)
                names[o["id"]] = o.get("thread_name")
            except (ValueError, KeyError):
                pass
    for path in glob.glob(f"{root}/sessions/**/rollout-*", recursive=True) + glob.glob(f"{root}/archived_sessions/**/rollout-*", recursive=True):
        m = ROLLOUT.match(os.path.basename(path))
        if not m:
            continue
        if path.endswith(".zst"):
            out = subprocess.run(["zstd", "-dc", path], capture_output=True)
            if out.returncode != 0:
                continue
            lines = out.stdout.decode("utf-8", "replace").splitlines()
        else:
            lines = open(path, encoding="utf-8", errors="replace")
        tid, main, request = m.group(1), True, None
        for line in lines:
            try:
                o = json.loads(line)
            except json.JSONDecodeError:
                continue
            p = o.get("payload") or {}
            if o.get("type") == "session_meta" and p.get("thread_source") not in (None, "user"):
                main = False
                break
            if request is None and o.get("type") == "event_msg" and p.get("type") == "user_message":
                text = p.get("message") or ""
                if text.strip() and not text.lstrip().startswith(SKIP) and not injected(text):
                    request = text
            if request is None and o.get("type") == "response_item" and p.get("type") == "message" and p.get("role") == "user":
                text = text_of(p.get("content"))
                if text.strip() and not text.lstrip().startswith(SKIP) and not injected(text):
                    request = text
            if request:
                break
        if main:
            emit("codex", tid, names.get(tid), request)


def t3_code():
    db_path = f"{HOME}/.t3/userdata/statev2.sqlite"
    if not os.path.exists(db_path):
        return
    db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    under = {}
    for thread, payload in db.execute("SELECT thread_id, payload_json FROM orchestration_v2_projection_provider_threads"):
        try:
            ref = json.loads(payload or "{}").get("nativeThreadRef") or {}
        except ValueError:
            continue
        source = {"claudeAgent": "claude-code", "codex": "codex"}.get(ref.get("driver"))
        if source and ref.get("nativeId"):
            under.setdefault(thread, set()).add(f"{source}/{ref['nativeId']}")
    first = {}
    for thread, payload in db.execute(
            "SELECT thread_id, payload_json FROM orchestration_v2_projection_messages WHERE role = 'user' ORDER BY created_at DESC"):
        try:
            first[thread] = json.loads(payload).get("text") or ""
        except ValueError:
            pass
    for thread, title in db.execute("SELECT thread_id, title FROM orchestration_v2_projection_threads"):
        emit("t3-code", thread, title, first.get(thread), under.get(thread, ()))
    db.close()


def grok():
    for folder in glob.glob(f"{HOME}/.grok/sessions/*/*/"):
        sid = os.path.basename(folder.rstrip("/"))
        if not UUID.fullmatch(sid):
            continue
        title = request = None
        try:
            summary = json.load(open(f"{folder}summary.json"))
            title = summary.get("generated_title") or summary.get("session_summary")
        except (OSError, ValueError):
            pass
        if os.path.exists(f"{folder}updates.jsonl"):
            for line in open(f"{folder}updates.jsonl", encoding="utf-8", errors="replace"):
                try:
                    u = (json.loads(line).get("params") or {}).get("update") or {}
                except (json.JSONDecodeError, AttributeError):
                    continue
                if u.get("sessionUpdate") == "user_message_chunk" and not (u.get("_meta") or {}).get("hideFromScrollback"):
                    content = u.get("content")
                    request = text_of([content] if isinstance(content, dict) else content)
                    if request.strip():
                        break
        if not (request or "").strip() and os.path.exists(f"{folder}chat_history.jsonl"):
            for line in open(f"{folder}chat_history.jsonl", encoding="utf-8", errors="replace"):
                try:
                    o = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if o.get("type") == "user" and not o.get("synthetic_reason"):
                    request = text_of(o.get("content"))
                    if request.strip():
                        break
        emit("grok", sid, title, request)


for collect in (claude_code, codex, t3_code, grok):
    collect()
