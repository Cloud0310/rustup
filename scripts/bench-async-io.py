#!/usr/bin/env python3
"""Paired, fresh-process extraction comparison; only Python stdlib is needed.

Build: cargo build --release --locked --example async-io-poc --features async-io-poc
Run:   python3 scripts/bench-async-io.py --work-dir /path/on/test/filesystem

Generated archives and every subprocess measurement remain in work-dir. A
nonzero exit means the proposed local parity gate failed, not a broken build.
This measures extraction, excluding network, validation and cleanup. Inputs are
warm-cache; outputs are fresh. Use --archive for real cached component archives.
"""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import statistics
import subprocess
import sys
import tarfile


class RepeatedBytes:
    def __init__(self, size):
        self.remaining = size

    def read(self, size=-1):
        size = self.remaining if size < 0 else min(size, self.remaining)
        self.remaining -= size
        return bytes(range(256)) * (size // 256) + bytes(range(size % 256))


def make_fixture(path, kind):
    with tarfile.open(path, "w", format=tarfile.USTAR_FORMAT) as archive:
        def directory(name):
            entry = tarfile.TarInfo(name)
            entry.type = tarfile.DIRTYPE
            entry.mode = 0o755
            archive.addfile(entry)

        def file(name, size):
            entry = tarfile.TarInfo(name)
            entry.size = size
            entry.mode = 0o644
            archive.addfile(entry, RepeatedBytes(size))

        directory("pkg")
        if kind == "docs":
            # 20,000 files, 1,000 directories, bucket sizes 4K and 8K.
            for parent in range(1000):
                directory(f"pkg/docs/{parent}") if parent else directory("pkg/docs")
                if parent == 0:
                    directory("pkg/docs/0")
                for child in range(20):
                    file(f"pkg/docs/{parent}/{child}.html", 1024 + child * 256)
        elif kind == "large":
            directory("pkg/lib")
            file("pkg/lib/llvm.so", 512 * 1024 * 1024 + 17)
        elif kind == "mixed":
            # Exercises every buffer bucket, empty files, streaming, and
            # implicit parents (with no repeated directory declaration).
            for size in (0, 1024, 8192, 9000, 1024 * 1024 + 1, 8 * 1024 * 1024 + 1):
                for index in range(20):
                    file(f"pkg/missing/{size}/{index}", size)
            file("pkg/missing/large", 48 * 1024 * 1024 + 3)
        else:
            raise ValueError(kind)


def upper_bootstrap_median(values):
    rng = random.Random(4159)
    samples = sorted(statistics.median(rng.choices(values, k=len(values))) for _ in range(10000))
    return samples[9499]  # one-sided 95% bound, paired ratios


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--archive", type=Path, action="append")
    parser.add_argument("--fixtures", nargs="+", default=["docs", "large", "mixed"])
    parser.add_argument("--threads", nargs="+", type=int, default=[1, 4, 8])
    parser.add_argument("--ram-mib", nargs="+", type=int, default=[32, 64])
    parser.add_argument("--rounds", type=int, default=9)
    parser.add_argument("--wall-tolerance", type=float, default=1.05)
    parser.add_argument("--cpu-tolerance", type=float, default=1.10)
    parser.add_argument("--rss-tolerance", type=float, default=1.10)
    args = parser.parse_args()
    if args.rounds < 5:
        parser.error("at least five paired rounds are required")
    work = args.work_dir.resolve()
    work.mkdir(parents=True, exist_ok=True)
    archives = args.archive or []
    if not archives:
        for kind in args.fixtures:
            path = work / f"{kind}.tar"
            make_fixture(path, kind)
            archives.append(path)
    else:
        archives = [path.resolve() for path in archives]
    if args.binary:
        binary = args.binary.resolve()
    else:
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"], text=True))
        binary = Path(metadata["target_directory"]) / "release/examples/async-io-poc"
    report = {
        "environment": {
            "platform": platform.platform(), "python": sys.version,
            "logical_cpus": os.cpu_count(), "work_dir": str(work),
            "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
            "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
            "binary": str(binary), "cache": "warm input; fresh output directory",
            "binary_sha256": hashlib.file_digest(binary.open("rb"), "sha256").hexdigest(),
            "cargo_lock_sha256": hashlib.sha256(Path("Cargo.lock").read_bytes()).hexdigest(),
        },
        "inputs": [{"path": str(path), "bytes": path.stat().st_size,
                    "sha256": hashlib.file_digest(path.open("rb"), "sha256").hexdigest()}
                   for path in archives],
        "gate": {"wall_upper95": args.wall_tolerance, "cpu_upper95": args.cpu_tolerance,
                 "rss_median_ratio": args.rss_tolerance},
        "cases": [],
    }
    raw = work / "measurements.jsonl"
    with raw.open("w") as measurements:
        for archive in archives:
            expected = None
            for threads in args.threads:
                for ram in args.ram_mib:
                    label = f"{archive.name}/threads={threads}/ram={ram}MiB"
                    pairs = []
                    for repetition in range(-1, args.rounds):
                        pair = {}
                        order = ["threaded", "tokio"] if repetition % 2 else ["tokio", "threaded"]
                        for backend in order:
                            command = [str(binary), "--backend", backend, "--archive", str(archive),
                                       "--output-parent", str(work), "--threads", str(threads),
                                       "--ram-mib", str(ram)]
                            result = subprocess.run(command, check=True, capture_output=True, text=True, timeout=300)
                            sample = json.loads(result.stdout)
                            signature = (sample["files"], sample["bytes"], sample["sha256"])
                            if expected is None:
                                expected = signature
                            if signature != expected:
                                raise RuntimeError(f"output mismatch: {label}/{backend}: {signature} != {expected}")
                            record = dict(case=label, backend=backend, repetition=repetition, **sample)
                            measurements.write(json.dumps(record) + "\n")
                            measurements.flush()
                            pair[backend] = sample
                        if repetition >= 0:
                            pairs.append(pair)
                    case = {"case": label, "rounds": args.rounds, "output": expected, "metrics": {}}
                    for metric in ("seconds", "cpu_seconds", "rss_kib"):
                        baseline = [pair["threaded"][metric] for pair in pairs]
                        candidate = [pair["tokio"][metric] for pair in pairs]
                        if min(baseline) <= 0:
                            raise RuntimeError(f"{metric} is unavailable; collect platform metrics before claiming parity")
                        ratios = [b / a for a, b in zip(baseline, candidate)]
                        case["metrics"][metric] = {
                            "threaded_median": statistics.median(baseline),
                            "tokio_median": statistics.median(candidate),
                            "paired_ratio_median": statistics.median(ratios),
                            "paired_ratio_upper95": upper_bootstrap_median(ratios),
                            "threaded_p95": sorted(baseline)[math.ceil(0.95 * len(baseline)) - 1],
                            "tokio_p95": sorted(candidate)[math.ceil(0.95 * len(candidate)) - 1],
                        }
                    metrics = case["metrics"]
                    case["pass"] = (metrics["seconds"]["paired_ratio_upper95"] <= args.wall_tolerance
                                    and metrics["cpu_seconds"]["paired_ratio_upper95"] <= args.cpu_tolerance
                                    and metrics["rss_kib"]["paired_ratio_median"] <= args.rss_tolerance)
                    report["cases"].append(case)
                    (work / "report.json").write_text(json.dumps(report, indent=2) + "\n")
                    print(label, "PASS" if case["pass"] else "FAIL",
                          "wall ratio", round(metrics["seconds"]["paired_ratio_median"], 3),
                          "upper95", round(metrics["seconds"]["paired_ratio_upper95"], 3), flush=True)
    return 0 if all(case["pass"] for case in report["cases"]) else 1


if __name__ == "__main__":
    sys.exit(main())
