#!/usr/bin/env python3
"""Download swe-bench-verified-mini (50 instances) to instances.jsonl.

Run with the local venv: .venv/bin/python fetch_dataset.py
"""
import hashlib
import json
import pathlib
import sys

DATASET = "MariusHobbhahn/swe-bench-verified-mini"
OUT = pathlib.Path(__file__).parent / "instances.jsonl"
EXPECTED_INSTANCES = 50
EXPECTED_SOURCE_FINGERPRINT = (
    "93185a2cce684b2b99592edeb83e16a47806c456fe335931cc92747601584a88"
)


def evaluator_image(instance_id: str) -> str:
    """Return the official SWE-bench image name for one instance."""
    from swebench.task.checks import expected_image

    return expected_image(instance_id)


def enrich_instances(rows: list[dict]) -> list[dict]:
    """Add runner metadata omitted by the source mini dataset."""
    enriched = []
    for row in rows:
        item = dict(row)
        item["image"] = evaluator_image(item["instance_id"])
        enriched.append(item)
    return enriched


def dataset_bytes(rows: list[dict]) -> bytes:
    return "".join(
        json.dumps(row, sort_keys=True, separators=(",", ":")) + "\n" for row in rows
    ).encode()


def dataset_fingerprint(rows: list[dict]) -> str:
    return hashlib.sha256(dataset_bytes(rows)).hexdigest()


def write_dataset(
    source_rows: list[dict],
    output: pathlib.Path,
    *,
    expected_count: int = EXPECTED_INSTANCES,
    expected_source_fingerprint: str = EXPECTED_SOURCE_FINGERPRINT,
) -> tuple[str, str]:
    if len(source_rows) != expected_count:
        raise ValueError(f"expected {expected_count} instances, got {len(source_rows)}")
    source_fingerprint = dataset_fingerprint(source_rows)
    if source_fingerprint != expected_source_fingerprint:
        raise ValueError(
            "source dataset fingerprint changed: "
            f"expected {expected_source_fingerprint}, got {source_fingerprint}"
        )

    enriched = enrich_instances(source_rows)
    enriched_fingerprint = dataset_fingerprint(enriched)
    temporary = output.with_suffix(output.suffix + ".tmp")
    try:
        temporary.write_bytes(dataset_bytes(enriched))
        temporary.replace(output)
    finally:
        temporary.unlink(missing_ok=True)
    return source_fingerprint, enriched_fingerprint


def main() -> None:
    from datasets import load_dataset

    ds = load_dataset(DATASET, split="test")
    source_rows = [dict(row) for row in ds]
    try:
        source_fingerprint, enriched_fingerprint = write_dataset(source_rows, OUT)
    except ValueError as error:
        sys.exit(str(error))
    print(f"wrote {len(source_rows)} instances to {OUT}")
    print(f"source sha256:   {source_fingerprint}")
    print(f"enriched sha256: {enriched_fingerprint}")


if __name__ == "__main__":
    main()
