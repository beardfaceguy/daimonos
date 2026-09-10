# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1450-swebench-restoration" -->

<!-- event id="request" artifact path="1450-swebench-restoration/artifacts/round-1-review-request.diff" sha256="da9015de121efc4f75c1b0f70183a581feb088ff2c9773c563aa0205f3122928" -->
## Review Request — Round 1
**Task:** 1450 — Restore and make the SWE-bench harness reproducible
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Pin the local Python/SWE-bench setup in documentation, replace an obsolete smoke instance, enrich every mini-dataset row with the official evaluator image name during the tracked fetch step, and regression-test the image mapping so Docker runs no longer depend on a lost local merge step.

### Relevant Code / Diff
diff --git i/benchmarks/swebench/README.md w/benchmarks/swebench/README.md
index 543960e..172187f 100644
--- i/benchmarks/swebench/README.md
+++ w/benchmarks/swebench/README.md
@@ -14,13 +14,21 @@ FAIL_TO_PASS / PASS_TO_PASS tests in a per-instance Docker image.
 ## One-time setup
 
 ```sh
-uv venv .venv
-uv pip install --python .venv/bin/python swebench
+uv venv --python 3.12 .venv
+uv pip install --python .venv/bin/python 'swebench==5.0.2'
 .venv/bin/python fetch_dataset.py      # writes instances.jsonl (50 rows)
 ```
 
 Evaluation additionally needs a working Docker daemon your user can talk to
 (`docker info` must succeed without sudo).
+After adding yourself to the `docker` group, log out and back in before testing
+access from an existing desktop session.
+
+Restoration reference (2026-09-09): uv 0.12.10, CPython 3.12.14,
+SWE-bench 5.0.2, and mini-dataset SHA-256
+`ea5be657c14d3377657de79fe175218f82d02670dc1ba8a14d3825687c3ce9e3`.
+`fetch_dataset.py` enriches every source row with the official evaluator image
+name required by the Docker runners.
 
 ## Running
 
@@ -30,10 +38,11 @@ Model/provider/key come from `~/.config/daimonos/agent.env`, exactly like
 
 ```sh
 # Single-instance smoke test FIRST (same cost policy as ../README.md):
-.venv/bin/python run_agent.py --filter astropy__astropy-14309 --tag smoke
+.venv/bin/python run_agent.py --docker \
+  --instance-ids django__django-11815 --tag smoke
 
 # Full 50-instance run:
-.venv/bin/python run_agent.py --tag <label>
+.venv/bin/python run_agent.py --docker --tag <label>
 ```
 
 Each run writes `results/<run-id>/` with per-instance token/cost JSONs
@@ -58,7 +67,7 @@ Zero-LLM-cost plumbing check (evaluates the dataset's own gold patches):
 .venv/bin/python -m swebench.harness.run_evaluation \
   --dataset_name SWE-bench/SWE-bench_Verified \
   --predictions_path gold \
-  --instance_ids astropy__astropy-14309 \
+  --instance_ids django__django-11815 \
   --max_workers 1 --run_id gold-smoke
 ```
 
diff --git i/benchmarks/swebench/fetch_dataset.py w/benchmarks/swebench/fetch_dataset.py
index 1dbaeb7..ab6ba7d 100644
--- i/benchmarks/swebench/fetch_dataset.py
+++ w/benchmarks/swebench/fetch_dataset.py
@@ -7,17 +7,30 @@ import json
 import pathlib
 import sys
 
-from datasets import load_dataset
-
 DATASET = "MariusHobbhahn/swe-bench-verified-mini"
 OUT = pathlib.Path(__file__).parent / "instances.jsonl"
 
 
+def evaluator_image(instance_id: str) -> str:
+    """Return the official SWE-bench image name for one instance."""
+    key = f"sweb.eval.x86_64.{instance_id}:latest".lower()
+    return f"swebench/{key}".replace("__", "_1776_")
+
+
+def enrich_instance(row: dict) -> dict:
+    """Add runner metadata omitted by the source mini dataset."""
+    enriched = dict(row)
+    enriched["image"] = evaluator_image(enriched["instance_id"])
+    return enriched
+
+
 def main() -> None:
+    from datasets import load_dataset
+
     ds = load_dataset(DATASET, split="test")
     with OUT.open("w") as f:
         for row in ds:
-            f.write(json.dumps(row) + "\n")
+            f.write(json.dumps(enrich_instance(row)) + "\n")
     print(f"wrote {len(ds)} instances to {OUT}")
     if len(ds) != 50:
         sys.exit(f"expected 50 instances, got {len(ds)}")
diff --git 1/tests/test_swebench_setup.py 2/tests/test_swebench_setup.py
new file mode 100644
index 0000000..597fad7
--- /dev/null
+++ 2/tests/test_swebench_setup.py
@@ -0,0 +1,36 @@
+"""Tests for reproducible SWE-bench dataset preparation."""
+
+from __future__ import annotations
+
+import importlib.util
+from pathlib import Path
+
+
+FETCH_SCRIPT = (
+    Path(__file__).resolve().parents[1] / "benchmarks" / "swebench" / "fetch_dataset.py"
+)
+
+
+def load_fetch_module():
+    spec = importlib.util.spec_from_file_location("fetch_dataset", FETCH_SCRIPT)
+    assert spec is not None and spec.loader is not None
+    module = importlib.util.module_from_spec(spec)
+    spec.loader.exec_module(module)
+    return module
+
+
+def test_enrich_instance_adds_official_evaluator_image():
+    fetch_dataset = load_fetch_module()
+
+    enriched = fetch_dataset.enrich_instance(
+        {
+            "instance_id": "django__django-11815",
+            "repo": "django/django",
+        }
+    )
+
+    assert enriched == {
+        "instance_id": "django__django-11815",
+        "repo": "django/django",
+        "image": "swebench/sweb.eval.x86_64.django_1776_django-11815:latest",
+    }

### Known Concerns
1. The official image-name formula is duplicated locally instead of imported from the pinned swebench package.
2. The documented dataset SHA includes the deterministic enrichment and therefore changes if serialization or source rows change.
3. The failed pre-enrichment smoke directory remains ignored local state.

### Specific Questions for Reviewer
1. Is image derivation faithful and stable for all 50 mini instances?
2. Are setup versions/fingerprint and Docker commands sufficiently reproducible?
3. Does the regression test cover the failure that blocked --docker?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. The evaluator image-name formula is hand-duplicated in fetch_dataset.py instead of being derived from the pinned swebench==5.0.2 package (e.g. via swebench.harness constants / test_spec image-key helpers). If upstream changes the namespace, tag, or the '__' -> '_1776_' encoding, the enrichment silently drifts from what the harness actually pulls. Import or wrap the official helper, or add a test that cross-checks the local formula against the pinned package's output for at least one instance.
B2. The regression test only checks the enrichment function's output against a hard-coded expected string — effectively restating the same duplicated formula. It does not cover the failure mode that blocked --docker (missing 'image' field in instances.jsonl consumed by the runner). Add a test asserting the written/enriched rows contain the 'image' key for every instance (or that the runner path fails loudly without it), so the lost-merge-step regression is actually guarded.
B3. The README documents a single mini-dataset SHA-256 that includes the local enrichment, so it changes whenever json serialization details or upstream source rows change, and readers cannot tell whether a mismatch means corrupted source data or a benign local change. Document the pre-enrichment (source) fingerprint separately, or have fetch_dataset.py verify the source rows' hash before enrichment and print/record the post-enrichment hash, clarifying which value is authoritative.

### Non-Blocking Suggestions
S1. The failed pre-enrichment smoke directory remains as ignored local state; add a note in the README (or a cleanup step) telling users to remove stale results/<run-id> smoke directories from before the enrichment fix so they are not mistaken for valid baselines.
S2. evaluator_image() lowercases the full key including instance_id; SWE-bench Verified instance ids are already lowercase, but consider asserting/normalizing explicitly (or adding a test with a mixed-case id) so the behavior is documented rather than incidental.
S3. fetch_dataset.py exits with a non-zero status for a wrong instance count only after writing instances.jsonl; consider validating len(ds) before writing (or writing to a temp file and renaming) so a bad fetch never leaves a plausible-looking dataset on disk.

**Verdict:** REVISE

<!-- event id="rebuttal-1" artifact path="1450-swebench-restoration/artifacts/round-1-rebuttal.diff" sha256="dce0253ac26c7ccd542f65b54af228ef8188482310b57551200da38797a47007" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — Valid. evaluator_image now delegates directly to swebench.task.checks.expected_image from the pinned SWE-bench 5.0.2 environment; no image formula remains duplicated locally. Live parity was checked for Django and Sphinx instances.
Re B2: ACCEPT — Valid. Enrichment now operates over the complete row list and tests assert every input row receives an image. write_dataset tests the actual serialized output and the live fetch regenerated all 50 rows with images.
Re B3: ACCEPT — Valid. Source rows are canonicalized and checked against a separate source fingerprint before writing. Enriched output has its own fingerprint. Output is written to a temporary file and atomically replaced only after count and source validation.

### Revised Code / Diff
diff --git i/benchmarks/swebench/README.md w/benchmarks/swebench/README.md
index 543960e..d3d228c 100644
--- i/benchmarks/swebench/README.md
+++ w/benchmarks/swebench/README.md
@@ -14,13 +14,27 @@ FAIL_TO_PASS / PASS_TO_PASS tests in a per-instance Docker image.
 ## One-time setup
 
 ```sh
-uv venv .venv
-uv pip install --python .venv/bin/python swebench
+uv venv --python 3.12 .venv
+uv pip install --python .venv/bin/python 'swebench==5.0.2'
 .venv/bin/python fetch_dataset.py      # writes instances.jsonl (50 rows)
 ```
 
 Evaluation additionally needs a working Docker daemon your user can talk to
 (`docker info` must succeed without sudo).
+After adding yourself to the `docker` group, log out and back in before testing
+access from an existing desktop session.
+
+Restoration reference (2026-09-09): uv 0.12.10, CPython 3.12.14,
+and SWE-bench 5.0.2. `fetch_dataset.py` verifies the canonical source rows
+before atomically replacing `instances.jsonl`:
+
+- source mini-dataset SHA-256:
+  `93185a2cce684b2b99592edeb83e16a47806c456fe335931cc92747601584a88`;
+- enriched `instances.jsonl` SHA-256:
+  `6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3`.
+
+The tracked fetch step uses SWE-bench's own pinned image-name helper to add the
+official evaluator image required by each Docker runner.
 
 ## Running
 
@@ -30,16 +44,21 @@ Model/provider/key come from `~/.config/daimonos/agent.env`, exactly like
 
 ```sh
 # Single-instance smoke test FIRST (same cost policy as ../README.md):
-.venv/bin/python run_agent.py --filter astropy__astropy-14309 --tag smoke
+.venv/bin/python run_agent.py --docker \
+  --instance-ids django__django-11815 --tag smoke
 
 # Full 50-instance run:
-.venv/bin/python run_agent.py --tag <label>
+.venv/bin/python run_agent.py --docker --tag <label>
 ```
 
 Each run writes `results/<run-id>/` with per-instance token/cost JSONs
 (same schema as the in-house suite — `../analyze.py results/` works),
 `.patch` files, raw transcripts, and `preds.jsonl`.
 
+Delete incomplete smoke directories created before dataset enrichment before
+treating `results/` as a baseline; a valid run contains `preds.jsonl`, a
+per-instance summary, token log, raw transcript, and non-empty patch.
+
 Repo checkouts are cached as bare clones under `repos/` (first run downloads
 each project once; django/astropy/sympy etc. total a few GB).
 
@@ -58,7 +77,7 @@ Zero-LLM-cost plumbing check (evaluates the dataset's own gold patches):
 .venv/bin/python -m swebench.harness.run_evaluation \
   --dataset_name SWE-bench/SWE-bench_Verified \
   --predictions_path gold \
-  --instance_ids astropy__astropy-14309 \
+  --instance_ids django__django-11815 \
   --max_workers 1 --run_id gold-smoke
 ```
 
diff --git i/benchmarks/swebench/fetch_dataset.py w/benchmarks/swebench/fetch_dataset.py
index 1dbaeb7..2954f56 100644
--- i/benchmarks/swebench/fetch_dataset.py
+++ w/benchmarks/swebench/fetch_dataset.py
@@ -3,24 +3,85 @@
 
 Run with the local venv: .venv/bin/python fetch_dataset.py
 """
+import hashlib
 import json
 import pathlib
 import sys
 
-from datasets import load_dataset
-
 DATASET = "MariusHobbhahn/swe-bench-verified-mini"
 OUT = pathlib.Path(__file__).parent / "instances.jsonl"
+EXPECTED_INSTANCES = 50
+EXPECTED_SOURCE_FINGERPRINT = (
+    "93185a2cce684b2b99592edeb83e16a47806c456fe335931cc92747601584a88"
+)
+
+
+def evaluator_image(instance_id: str) -> str:
+    """Return the official SWE-bench image name for one instance."""
+    from swebench.task.checks import expected_image
+
+    return expected_image(instance_id)
+
+
+def enrich_instances(rows: list[dict]) -> list[dict]:
+    """Add runner metadata omitted by the source mini dataset."""
+    enriched = []
+    for row in rows:
+        item = dict(row)
+        item["image"] = evaluator_image(item["instance_id"])
+        enriched.append(item)
+    return enriched
+
+
+def dataset_bytes(rows: list[dict]) -> bytes:
+    return "".join(
+        json.dumps(row, sort_keys=True, separators=(",", ":")) + "\n" for row in rows
+    ).encode()
+
+
+def dataset_fingerprint(rows: list[dict]) -> str:
+    return hashlib.sha256(dataset_bytes(rows)).hexdigest()
+
+
+def write_dataset(
+    source_rows: list[dict],
+    output: pathlib.Path,
+    *,
+    expected_count: int = EXPECTED_INSTANCES,
+    expected_source_fingerprint: str = EXPECTED_SOURCE_FINGERPRINT,
+) -> tuple[str, str]:
+    if len(source_rows) != expected_count:
+        raise ValueError(f"expected {expected_count} instances, got {len(source_rows)}")
+    source_fingerprint = dataset_fingerprint(source_rows)
+    if source_fingerprint != expected_source_fingerprint:
+        raise ValueError(
+            "source dataset fingerprint changed: "
+            f"expected {expected_source_fingerprint}, got {source_fingerprint}"
+        )
+
+    enriched = enrich_instances(source_rows)
+    enriched_fingerprint = dataset_fingerprint(enriched)
+    temporary = output.with_suffix(output.suffix + ".tmp")
+    try:
+        temporary.write_bytes(dataset_bytes(enriched))
+        temporary.replace(output)
+    finally:
+        temporary.unlink(missing_ok=True)
+    return source_fingerprint, enriched_fingerprint
 
 
 def main() -> None:
+    from datasets import load_dataset
+
     ds = load_dataset(DATASET, split="test")
-    with OUT.open("w") as f:
-        for row in ds:
-            f.write(json.dumps(row) + "\n")
-    print(f"wrote {len(ds)} instances to {OUT}")
-    if len(ds) != 50:
-        sys.exit(f"expected 50 instances, got {len(ds)}")
+    source_rows = [dict(row) for row in ds]
+    try:
+        source_fingerprint, enriched_fingerprint = write_dataset(source_rows, OUT)
+    except ValueError as error:
+        sys.exit(str(error))
+    print(f"wrote {len(source_rows)} instances to {OUT}")
+    print(f"source sha256:   {source_fingerprint}")
+    print(f"enriched sha256: {enriched_fingerprint}")
 
 
 if __name__ == "__main__":
diff --git 1/tests/test_swebench_setup.py 2/tests/test_swebench_setup.py
new file mode 100644
index 0000000..c470106
--- /dev/null
+++ 2/tests/test_swebench_setup.py
@@ -0,0 +1,112 @@
+"""Tests for reproducible SWE-bench dataset preparation."""
+
+from __future__ import annotations
+
+import importlib.util
+import json
+from pathlib import Path
+
+import pytest
+
+
+FETCH_SCRIPT = (
+    Path(__file__).resolve().parents[1] / "benchmarks" / "swebench" / "fetch_dataset.py"
+)
+
+
+def load_fetch_module():
+    spec = importlib.util.spec_from_file_location("fetch_dataset", FETCH_SCRIPT)
+    assert spec is not None and spec.loader is not None
+    module = importlib.util.module_from_spec(spec)
+    spec.loader.exec_module(module)
+    return module
+
+
+def test_enrich_instances_adds_image_to_every_row(monkeypatch):
+    fetch_dataset = load_fetch_module()
+    monkeypatch.setattr(
+        fetch_dataset,
+        "evaluator_image",
+        lambda instance_id: f"official/{instance_id}",
+    )
+
+    enriched = fetch_dataset.enrich_instances(
+        [
+            {"instance_id": "django__django-11815"},
+            {"instance_id": "sphinx-doc__sphinx-8035"},
+        ]
+    )
+
+    assert [row["image"] for row in enriched] == [
+        "official/django__django-11815",
+        "official/sphinx-doc__sphinx-8035",
+    ]
+
+
+def test_evaluator_image_matches_pinned_swebench_helper():
+    checks = pytest.importorskip("swebench.task.checks")
+    fetch_dataset = load_fetch_module()
+
+    assert fetch_dataset.evaluator_image("django__django-11815") == checks.expected_image(
+        "django__django-11815"
+    )
+
+
+def test_write_dataset_rejects_invalid_source_before_replacing(tmp_path):
+    fetch_dataset = load_fetch_module()
+    output = tmp_path / "instances.jsonl"
+    output.write_text("existing\n")
+    row = {
+        "instance_id": "django__django-11815",
+        "repo": "django/django",
+    }
+
+    with pytest.raises(ValueError, match="expected 2 instances"):
+        fetch_dataset.write_dataset(
+            [row],
+            output,
+            expected_count=2,
+            expected_source_fingerprint=fetch_dataset.dataset_fingerprint([row]),
+        )
+    assert output.read_text() == "existing\n"
+
+    with pytest.raises(ValueError, match="source dataset fingerprint changed"):
+        fetch_dataset.write_dataset(
+            [row],
+            output,
+            expected_count=1,
+            expected_source_fingerprint="not-the-source-fingerprint",
+        )
+    assert output.read_text() == "existing\n"
+
+
+def test_write_dataset_is_atomic_and_reports_both_fingerprints(tmp_path, monkeypatch):
+    fetch_dataset = load_fetch_module()
+    monkeypatch.setattr(
+        fetch_dataset,
+        "evaluator_image",
+        lambda instance_id: f"official/{instance_id}",
+    )
+    output = tmp_path / "instances.jsonl"
+    source = [
+        {
+            "instance_id": "django__django-11815",
+            "repo": "django/django",
+        }
+    ]
+    source_fingerprint = fetch_dataset.dataset_fingerprint(source)
+
+    fingerprints = fetch_dataset.write_dataset(
+        source,
+        output,
+        expected_count=1,
+        expected_source_fingerprint=source_fingerprint,
+    )
+
+    written = [json.loads(line) for line in output.read_text().splitlines()]
+    assert written[0]["image"] == "official/django__django-11815"
+    assert fingerprints == (
+        source_fingerprint,
+        fetch_dataset.dataset_fingerprint(written),
+    )
+    assert not output.with_suffix(".jsonl.tmp").exists()

### New Points
R1. S1 and S3 were adopted: README explains stale pre-enrichment result cleanup, and invalid counts/fingerprints cannot replace an existing dataset. S2 is delegated to the official helper rather than locally normalizing case.
R2. Portable tests pass (3 passed, 1 dependency-gated skip); the installed pinned SWE-bench environment confirms official helper parity for 2/2 representative instances; live fetch validates 50/50 images and both fingerprints.

**Requesting:** RE-REVIEW

<!-- event id="review-2" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S4. test_write_dataset_is_atomic_and_reports_both_fingerprints checks output.with_suffix(".jsonl.tmp"), but Path.with_suffix replaces the existing suffix rather than appending, so the asserted temp path differs from the one write_dataset actually creates (instances.jsonl.tmp via with_suffix(output.suffix + ".tmp")). Compute the temp path the same way as the implementation (or assert no *.tmp files remain in tmp_path) so the leftover-temp-file assertion actually guards the intended path.
S5. Consider noting in the README (or a comment near EXPECTED_SOURCE_FINGERPRINT) that the source fingerprint is computed over canonicalized rows (sorted keys, compact separators) rather than the upstream file bytes, so future maintainers regenerating the value use the same canonicalization.

### Responses to Prior Points
Re R1: Acknowledged. S1 is addressed by the README guidance on deleting incomplete pre-enrichment smoke directories and the description of what a valid run contains. S3 is addressed by write_dataset validating count and source fingerprint before writing, plus the temp-file-and-atomic-replace pattern with a test proving the existing file is untouched on failure. Delegating S2 to the official expected_image helper is a reasonable resolution — case handling is now upstream's contract, and the parity test guards against drift.
Re R2: Acknowledged. The test evidence is convincing: the dependency-gated importorskip keeps the suite portable while the pinned environment exercises the real helper parity check, and the live fetch validating 50/50 images plus both fingerprints confirms end-to-end behavior. No further verification requested.
B1: resolved
B2: resolved
B3: resolved

**Verdict:** APPROVE
