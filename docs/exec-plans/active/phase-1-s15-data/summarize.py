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
    python docs/exec-plans/active/phase-1-s15-data/summarize.py --runs [--skip N] <f.json>
    python docs/exec-plans/active/phase-1-s15-data/summarize.py --gui <file.json> ...
    python docs/exec-plans/active/phase-1-s15-data/summarize.py --drains <file.json> ...
    python docs/exec-plans/active/phase-1-s15-data/summarize.py --stream <file.json> ...
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


def gui_table(paths):
    """The GUI thread's share of a frame, next to the render thread's.

    `gui` is previous swap -> this frame's `afterAnimating`: TableView's
    polish, delegate creation/reuse and every `data()` call. On a platform
    whose `requestUpdate()` waits on an idle timer that wait is inside it, so
    read this against a run with `QT_QPA_UPDATE_IDLE_TIME=0`.
    """
    print(
        f'{"run":22s} {"phase":12s} {"n":>5s} {"gui p50":>8s} {"gui p99":>8s} '
        f'{"gui max":>8s} {"sync p50":>8s} {"hand p50":>8s} {"sg p50":>8s} '
        f'{"sg p99":>8s} {"gui+sg p50":>10s}'
    )
    for path in paths:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
        label = data["environment"].get("label") or path
        for phase in data.get("phases", []):
            g = phase.get("guiFrame", {})
            s = phase.get("sync", {})
            h = phase.get("guiHandover", {})
            w = phase.get("renderWork", {})
            print(
                f'{label:22s} {phase["phase"]:12s} {g.get("count", 0):5d} '
                f'{ms(g.get("p50Ns")):8.3f} {ms(g.get("p99Ns")):8.3f} '
                f'{ms(g.get("maxNs")):8.3f} {ms(s.get("p50Ns")):8.3f} '
                f'{ms(h.get("p50Ns")):8.3f} {ms(w.get("p50Ns")):8.3f} '
                f'{ms(w.get("p99Ns")):8.3f} '
                f'{ms(g.get("p50Ns")) + ms(w.get("p50Ns")):10.3f}'
            )


def drain_table(paths):
    """K4: what one drain of the hub's event queue costs the UI thread.

    `boundary` is the time inside `reldex_hub_next_event` plus taking
    ownership of what the event carried; the rest of `drain` is Qt model/view
    work (routing, applyBatch, endInsertRows and its signals).
    """
    print(
        f'{"run":22s} {"phase":12s} {"n":>6s} {"ev":>7s} {"drain p50":>9s} '
        f'{"p99":>8s} {"max":>9s} {"bnd p50":>8s} {"bnd p99":>8s} '
        f'{"batch p50":>9s} {"batch p99":>9s} {"batch n":>7s}'
    )

    def row(label, name, d, b, a):
        print(
            f'{label:22s} {name:12s} {d.get("count", 0):6d} '
            f'{d.get("totalEvents", 0):7d} {ms(d.get("p50Ns")) * 1000:9.1f} '
            f'{ms(d.get("p99Ns")) * 1000:8.1f} {ms(d.get("maxNs")) * 1000:9.1f} '
            f'{ms(b.get("p50Ns")) * 1000:8.1f} {ms(b.get("p99Ns")) * 1000:8.1f} '
            f'{ms(a.get("p50Ns")) * 1000:9.1f} {ms(a.get("p99Ns")) * 1000:9.1f} '
            f'{a.get("count", 0):7d}'
        )

    print("(all times in microseconds)")
    for path in paths:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
        label = data["environment"].get("label") or path
        stream = data.get("initialStream", {})
        if stream:
            row(
                label,
                "1M stream",
                stream.get("drains", {}),
                stream.get("drainBoundary", {}),
                stream.get("applyBatch", {}),
            )
        for phase in data.get("phases", []):
            if phase.get("drains", {}).get("count", 0) == 0:
                continue
            row(
                label,
                phase["phase"],
                phase.get("drains", {}),
                phase.get("drainBoundary", {}),
                phase.get("applyBatch", {}),
            )


def stream_table(paths):
    """K6's "or any other session": a third hub's execute -> resultComplete."""
    print(
        f'{"run":22s} {"phase":14s} {"n":>3s} {"rows":>9s} {"min ms":>8s} '
        f'{"median":>8s} {"max ms":>8s}   each (ms)'
    )
    for path in paths:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
        label = data["environment"].get("label") or path
        for phase in data.get("phases", []):
            values = sorted(ms(v) for v in phase.get("streamNs", []))
            if not values:
                continue
            each = " ".join(f"{v:.1f}" for v in sorted(ms(v) for v in phase["streamNs"]))
            print(
                f'{label:22s} {phase["phase"]:14s} {len(values):3d} '
                f'{phase.get("streamRows", 0):9d} {values[0]:8.1f} '
                f"{values[len(values) // 2]:8.1f} {values[-1]:8.1f}   {each}"
            )


def run_table(paths, skip=0):
    """K2: execute submitted -> first rows inserted -> first frame swapped."""
    samples = []
    dropped = 0
    for path in paths:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
        runs = data.get("runs", [])
        if skip:
            dropped += min(skip, len(runs))
            runs = runs[skip:]
        for run in runs:
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
    if dropped:
        # Printed, not silent: "the 2nd..31st execute" is a claim about which
        # samples were used, and a reader must be able to check it.
        print(f"--skip {skip}: dropped {dropped} run(s) before aggregating")
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
    args = argv[1:]
    mode = "phases"
    skip = 0
    paths = []
    index = 0
    while index < len(args):
        arg = args[index]
        if arg in ("--runs", "--gui", "--drains", "--stream"):
            mode = arg[2:]
        elif arg == "--skip":
            index += 1
            skip = int(args[index])
        else:
            paths.append(arg)
        index += 1
    if mode == "runs":
        run_table(paths, skip)
    elif mode == "gui":
        gui_table(paths)
    elif mode == "drains":
        drain_table(paths)
    elif mode == "stream":
        stream_table(paths)
    else:
        phase_table(paths)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
