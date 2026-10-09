#!/usr/bin/env python3
"""Compare perf JSONL results against scripts/e2e/baselines/<probe>.json.

    python3 scripts/e2e/compare_baseline.py RESULTS.jsonl [...] [--threshold 0.25] [--commit SHA] [--write]

Exit code = number of probes with a regression (median > baseline * 1.25).
--write stores the current medians as the new baseline for every probe seen.
"""
import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from perf import compare_baseline, format_comparison, medians_from_jsonl, write_baseline  # noqa: E402


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("results", nargs="+")
    ap.add_argument("--threshold", type=float, default=0.25)
    ap.add_argument("--commit", help="only use records from this commit")
    ap.add_argument("--write", action="store_true", help="write current medians as baselines")
    a = ap.parse_args()
    probes = {}
    for path in a.results:
        for probe, metrics in medians_from_jsonl(path, a.commit).items():
            probes.setdefault(probe, {}).update(metrics)
    failures = 0
    for probe, metrics in sorted(probes.items()):
        if a.write:
            write_baseline(probe, metrics)
            print(f"{probe}: baseline written ({len(metrics)} metrics)")
            continue
        result = compare_baseline(probe, metrics, a.threshold)
        print(format_comparison(result))
        failures += 0 if result["ok"] else 1
    return failures


if __name__ == "__main__":
    sys.exit(main())
