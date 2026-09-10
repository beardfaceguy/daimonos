"""Tests for reproducible SWE-bench dataset preparation."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path

import pytest


FETCH_SCRIPT = (
    Path(__file__).resolve().parents[1] / "benchmarks" / "swebench" / "fetch_dataset.py"
)


def load_fetch_module():
    spec = importlib.util.spec_from_file_location("fetch_dataset", FETCH_SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_enrich_instances_adds_image_to_every_row(monkeypatch):
    fetch_dataset = load_fetch_module()
    monkeypatch.setattr(
        fetch_dataset,
        "evaluator_image",
        lambda instance_id: f"official/{instance_id}",
    )

    enriched = fetch_dataset.enrich_instances(
        [
            {"instance_id": "django__django-11815"},
            {"instance_id": "sphinx-doc__sphinx-8035"},
        ]
    )

    assert [row["image"] for row in enriched] == [
        "official/django__django-11815",
        "official/sphinx-doc__sphinx-8035",
    ]


def test_evaluator_image_matches_pinned_swebench_helper():
    checks = pytest.importorskip("swebench.task.checks")
    fetch_dataset = load_fetch_module()

    assert fetch_dataset.evaluator_image("django__django-11815") == checks.expected_image(
        "django__django-11815"
    )


def test_write_dataset_rejects_invalid_source_before_replacing(tmp_path):
    fetch_dataset = load_fetch_module()
    output = tmp_path / "instances.jsonl"
    output.write_text("existing\n")
    row = {
        "instance_id": "django__django-11815",
        "repo": "django/django",
    }

    with pytest.raises(ValueError, match="expected 2 instances"):
        fetch_dataset.write_dataset(
            [row],
            output,
            expected_count=2,
            expected_source_fingerprint=fetch_dataset.dataset_fingerprint([row]),
        )
    assert output.read_text() == "existing\n"

    with pytest.raises(ValueError, match="source dataset fingerprint changed"):
        fetch_dataset.write_dataset(
            [row],
            output,
            expected_count=1,
            expected_source_fingerprint="not-the-source-fingerprint",
        )
    assert output.read_text() == "existing\n"


def test_write_dataset_is_atomic_and_reports_both_fingerprints(tmp_path, monkeypatch):
    fetch_dataset = load_fetch_module()
    monkeypatch.setattr(
        fetch_dataset,
        "evaluator_image",
        lambda instance_id: f"official/{instance_id}",
    )
    output = tmp_path / "instances.jsonl"
    source = [
        {
            "instance_id": "django__django-11815",
            "repo": "django/django",
        }
    ]
    source_fingerprint = fetch_dataset.dataset_fingerprint(source)

    fingerprints = fetch_dataset.write_dataset(
        source,
        output,
        expected_count=1,
        expected_source_fingerprint=source_fingerprint,
    )

    written = [json.loads(line) for line in output.read_text().splitlines()]
    assert written[0]["image"] == "official/django__django-11815"
    assert fingerprints == (
        source_fingerprint,
        fetch_dataset.dataset_fingerprint(written),
    )
    assert not output.with_suffix(".jsonl.tmp").exists()
