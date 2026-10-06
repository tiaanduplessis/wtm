#!/usr/bin/env python3
"""Compare binaries against the same isolated, fully recursive Git fixture."""

import argparse
import json
import os
from pathlib import Path
import statistics
import subprocess
import tempfile
import time


def git(repo, *args):
    subprocess.run(["git", "-C", str(repo), *map(str, args)], check=True, capture_output=True,
                   env={**os.environ, "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"})


def snapshot(report):
    fields = ("path", "head", "short_head", "branch", "status", "updated_at", "committed_at",
              "commit_subject", "upstream", "ahead", "behind", "merged", "locked", "prunable")
    return sorted(({key: row[key] for key in fields} for row in report["worktrees"]), key=lambda row: row["path"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("optimized", type=Path)
    parser.add_argument("--repositories", type=int, default=12)
    parser.add_argument("--worktrees", type=int, default=4, help="including each primary checkout")
    parser.add_argument("--files", type=int, default=200)
    parser.add_argument("--repeats", type=int, default=3)
    args = parser.parse_args()
    if min(args.repositories, args.worktrees, args.files, args.repeats) < 1:
        parser.error("fixture sizes and repeat count must be positive")
    binaries = {"baseline": args.baseline.resolve(), "optimized": args.optimized.resolve()}
    samples = {key: [] for key in binaries}
    expected = None
    with tempfile.TemporaryDirectory(prefix="wtm-benchmark-") as directory:
        root = Path(directory).resolve()
        for number in range(args.repositories):
            repo = root / "repositories" / f"repo-{number}"
            repo.mkdir(parents=True)
            git(repo, "init", "-b", "main")
            git(repo, "config", "user.name", "Benchmark Fixture")
            git(repo, "config", "user.email", "fixture@example.invalid")
            git(repo, "config", "commit.gpgsign", "false")
            for index in range(args.files):
                (repo / f"file-{index}").write_text(f"content {index}\n")
            git(repo, "add", ".")
            git(repo, "commit", "-m", "initial")
            for index in range(1, args.worktrees):
                git(repo, "worktree", "add", "-b", f"topic-{index}", root / ".worktrees" / f"repo-{number}-{index}")
        for repeat in range(args.repeats):
            order = list(binaries) if repeat % 2 == 0 else list(reversed(binaries))
            for name in order:
                start = time.monotonic()
                output = subprocess.run([str(binaries[name]), "list", str(root), "--json"], capture_output=True, check=True)
                elapsed = round((time.monotonic() - start) * 1000)
                report = json.loads(output.stdout)
                assert report["discovery_complete"] and not report["warnings"], report["warnings"]
                assert len(report["worktrees"]) == args.repositories * args.worktrees
                rows = snapshot(report)
                if expected is None:
                    expected = rows
                assert rows == expected, f"{name} changed inventory or metadata"
                samples[name].append({"wall_ms": elapsed, **report.get("profile", {})})
        result = {}
        for name, values in samples.items():
            result[name] = {key: statistics.median(value[key] for value in values)
                            for key in ("wall_ms", "git_commands", "git_elapsed_ms", "activity_ms", "commit_cache_hits")
                            if all(key in value for value in values)}
            first = [value["first_result_ms"] for value in values if value.get("first_result_ms") is not None]
            if first:
                result[name]["first_result_ms"] = statistics.median(first)
        result["fixture"] = {"repositories": args.repositories, "worktrees": args.repositories * args.worktrees,
                             "tracked_files_per_worktree": args.files, "repeats": args.repeats,
                             "identical_inventory": True}
        result["speedup"] = round(result["baseline"]["wall_ms"] / result["optimized"]["wall_ms"], 2)
        print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
