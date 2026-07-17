#!/usr/bin/env python3
"""Merge agentdeck discovery hooks into ~/.claude/settings.json, preserving existing hooks."""
import json, shutil, sys, os

path = os.path.expanduser("~/.claude/settings.json")
shutil.copy(path, path + ".agentdeck-backup")

with open(path) as f:
    settings = json.load(f)

URL = "http://127.0.0.1:4747/hook"
http_hook = {"type": "http", "url": URL, "timeout": 5}
def is_ours(e):
    return any(URL in h.get("url", "") or URL in h.get("command", "")
               for h in e.get("hooks", []))

def entry(hooks, matcher=None):
    e = {"hooks": hooks}
    if matcher is not None:
        e["matcher"] = matcher
    return e

http_events = [
    ("SubagentStart", None),
    ("SubagentStop", None),
    ("UserPromptSubmit", None),
    ("Stop", None),
    ("StopFailure", None),
    ("Notification", None),
    ("PostToolUse", "Agent"),
    ("SessionEnd", None),
    ("PreToolUse", "Agent"),  # discovery only: observe Agent tool inputs
]

hooks = settings.setdefault("hooks", {})
for event, matcher in http_events:
    lst = hooks.setdefault(event, [])
    lst[:] = [e for e in lst if not is_ours(e)]
    lst.append(entry([dict(http_hook)], matcher))

# SessionStart doesn't support type:http — command hook that curls the payload through
curl = ("curl -s --max-time 1 -X POST -H 'Content-Type: application/json' "
        f"--data-binary @- {URL} > /dev/null 2>&1; exit 0")
lst = hooks.setdefault("SessionStart", [])
lst[:] = [e for e in lst if not is_ours(e)]
lst.append(entry([{"type": "command", "command": curl, "timeout": 5}]))

with open(path, "w") as f:
    json.dump(settings, f, indent=2)
    f.write("\n")
print("merged. events:", sorted(hooks.keys()))
