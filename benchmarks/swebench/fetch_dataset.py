#!/usr/bin/env python3
"""Download swe-bench-verified-mini (50 instances) to instances.jsonl.

Run with the local venv: .venv/bin/python fetch_dataset.py
"""
import json
import pathlib
import sys

from datasets import load_dataset

DATASET = "MariusHobbhahn/swe-bench-verified-mini"
OUT = pathlib.Path(__file__).parent / "instances.jsonl"


def main() -> None:
    ds = load_dataset(DATASET, split="test")
    with OUT.open("w") as f:
        for row in ds:
            f.write(json.dumps(row) + "\n")
    print(f"wrote {len(ds)} instances to {OUT}")
    if len(ds) != 50:
        sys.exit(f"expected 50 instances, got {len(ds)}")


if __name__ == "__main__":
    main()
