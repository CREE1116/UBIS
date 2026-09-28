#!/usr/bin/env python3
"""Build a task file for `ubis-bench --tasks` from merged GitHub PRs.

Each task is what an agent would be handed ("fix this issue"): the PR title
and body as the query, the PR's parent commit as the tree to index, and the
units the PR changed as the answer.

    fetch_pr_tasks.py OWNER/REPO LOCAL_CLONE OUT.jsonl [--limit 200]

Needs `gh` (authenticated). PRs whose merge commit is not in the local clone
are skipped. Output lines: {"id", "base", "head", "text"}, oldest first.
"""
import json, re, subprocess, sys

def main():
    repo, clone, out = sys.argv[1:4]
    limit = sys.argv[sys.argv.index("--limit") + 1] if "--limit" in sys.argv else "200"
    prs = json.loads(subprocess.check_output(
        ["gh", "pr", "list", "-R", repo, "--state", "merged", "--limit", limit,
         "--json", "number,title,body,mergeCommit,mergedAt,author"]))
    tasks = []
    for pr in prs:
        author = pr.get("author") or {}
        if author.get("is_bot") or "[bot]" in author.get("login", "") or author.get("login", "").startswith("app/"):
            continue  # dependency bumps are not tasks an agent is handed
        head = (pr.get("mergeCommit") or {}).get("oid")
        if not head:
            continue
        r = subprocess.run(["git", "-C", clone, "rev-parse", "--verify", "-q", head + "^1"],
                           capture_output=True, text=True)
        if r.returncode != 0:
            continue
        body = pr.get("body") or ""
        body = re.sub(r"<!--.*?-->", "", body, flags=re.S)  # PR template comments
        body = body.strip()[:2000]
        tasks.append({"id": f"#{pr['number']}", "at": pr["mergedAt"], "base": r.stdout.strip(),
                      "head": head, "text": (pr["title"] + "\n\n" + body).strip()})
    tasks.sort(key=lambda t: t["at"])
    with open(out, "w") as f:
        for t in tasks:
            del t["at"]
            f.write(json.dumps(t, ensure_ascii=False) + "\n")
    print(f"{len(tasks)} tasks -> {out}", file=sys.stderr)

if __name__ == "__main__":
    main()
