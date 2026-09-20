#!/usr/bin/env python3
"""Summarise the JSON a spike S15 measurement run writes (M1.8).

The Reldex app is a GUI-subsystem binary whose stdout does not reach a pipe on
this machine (ui/README.md), so `ScrollDriver` writes its numbers to the file
named by RELDEX_S15_OUT. This turns one or more of those files into the tables
that go into `phase-1-s15-ffi-spike.md`.

It computes nothing: every percentile in the output was produced by
`Metrics::frameStats()` inside the process that was measured. This only
selects, converts nanoseconds to milliseconds, and prints.

    python docs/exec-plans/active/phase-1-s15-data/summarize.py <file.json> ...
    python docs/exec-plans/active/phase-1-s15-data/summarize.py --runs <file.json>
"""

from __future__ import annotations

import json
import sys


def ms(ns):
    return float(ns) / 1e6 if ns is not None and ns >= 0 else float("nan")


def phase_table(paths):
    print(
        f'{"run":22s} {"phase":10s} {"n":>5s} {"p50":>7s} {"p90":>7s} {"p95":>7s} '
        f'{"p99":>7s} {"max":>8s} {">8ms":>6s} {">16.7":>6s} {"sgwork p50":>10s} '
        f'{"sgwork p99":>10s} {"rows":>10s}'
    )
    for path in paths:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
        label = data["environment"].get("label") or path
        for phase in data.get("phases", []):
            f = phase["frameIntervals"]
            w = phase["renderWork"]
            print(
                f'{label:22s} {phase["phase"]:10s} {f["count"]:5d} '
                f'{ms(f.get("p50Ns")):7.2f} {ms(f.get("p90Ns")):7.2f} '
                f'{ms(f.get("p95Ns")):7.2f} {ms(f.get("p99Ns")):7.2f} '
                f'{ms(f.get("maxNs")):8.2f} {f.get("over8ms", 0):6d} '
                f'{f.get("over16_7ms", 0):6d} {ms(w.get("p50Ns")):10.3f} '
                f'{ms(w.get("p99Ns")):10.3f} {phase.get("rowsTravelled", 0):10.0f}'
            )


def run_table(paths):
    """K2: execute submitted -> first rows inserted -> first frame swapped."""
    samples = []
    for path in paths:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
        for run in data.get("runs", []):
            submitted = run["executeSubmittedNs"]
            if submitted < 0 or run["firstFrameAfterInsertNs"] < 0:
                continue
            samples.append(
                (
                    ms(run["firstEventNs"] - submitted),
                    ms(run["firstRowsInsertedNs"] - submitted),
                    ms(run["firstFrameAfterInsertNs"] - submitted),
                )
            )
    if not samples:
        print("no complete runs found")
        return
    names = ("first event", "first rows inserted", "first frame swapped")
    print(f'{"stage":22s} {"n":>4s} {"p50":>8s} {"p95":>8s} {"max":>8s} {"mean":>8s}')
    for index, name in enumerate(names):
        values = sorted(sample[index] for sample in samples)
        count = len(values)
        print(
            f"{name:22s} {count:4d} "
            f"{values[int(0.50 * count)]:8.2f} {values[min(int(0.95 * count), count - 1)]:8.2f} "
            f"{values[-1]:8.2f} {sum(values) / count:8.2f}"
        )


def main(argv):
    if len(argv) > 1 and argv[1] == "--runs":
        run_table(argv[2:])
    else:
        phase_table(argv[1:])
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
