"""WP07 (FR-04 transport, requirements 1-13) acceptance tests.

Context: docs/adr/025-controlled-model-transport.md; the reviewer-runtime
INTERFACES ledger row for WP07. These exercise the opt-in controlled model
transport through the real `daimonos` CLI, without FR-01 (the capability probe,
WP09) or the audit record (WP08).

Scope here is what is observable through the process WITHOUT a model request:
the `--model-profile` startup validation (AC-04.7 and the no-mixing rule), and
the default-off opt-out regression. The exact body/header/retry/redirect
behavior of the transport itself (AC-04.1-04.5) is asserted at the transport
level by `cargo test providers::controlled_transport_tests`; the integrated
probe repeats them end to end in WP09.

No live credentialed endpoint is used anywhere.
"""

import json
import subprocess

import pytest

# Dry-run never calls a provider, loads no agent env, and opens no socket, so
# these complete fast; a generous ceiling still guards against a hang.
TIMEOUT = 60


def _run_agent(daimonos_binary, workspace, *extra, task="inspect the workspace"):
    return subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "agent", "--dry-run", *extra, task],
        capture_output=True,
        text=True,
        timeout=TIMEOUT,
    )


def _write_profile(path, **overrides):
    profile = {
        "provider_label": "anthropic",
        "model": "claude-opus-5-5",
        "base_url": "http://127.0.0.1:1",
        "wire": "messages",
        "credential_env": "DBUNK_CTRL_KEY",
    }
    profile.update(overrides)
    # Allow deletion of required keys to build partial profiles.
    for key in [k for k, v in list(profile.items()) if v is _MISSING]:
        del profile[key]
    path.write_text(json.dumps(profile))
    return path


class _Missing:
    pass


_MISSING = _Missing()


def test_agent_help_lists_model_profile(daimonos_binary):
    completed = subprocess.run(
        [daimonos_binary, "agent", "--help"],
        capture_output=True,
        text=True,
        timeout=5,
        check=True,
    )
    assert "--model-profile" in completed.stdout


def test_partial_profile_fails_at_startup_naming_missing_values(daimonos_binary, tmp_path):
    # AC-04.7: supplying only three of the five values fails at startup, naming
    # every missing value, before any request.
    profile = _write_profile(
        tmp_path / "partial.json",
        base_url=_MISSING,
        credential_env=_MISSING,
    )
    result = _run_agent(daimonos_binary, tmp_path, "--model-profile", str(profile))
    assert result.returncode != 0, result.stdout
    combined = result.stderr + result.stdout
    assert "missing" in combined, combined
    assert "base_url" in combined, combined
    assert "credential_env" in combined, combined


def test_single_missing_value_is_named(daimonos_binary, tmp_path):
    # AC-04.7, the four-of-five case: exactly the missing value is named.
    profile = _write_profile(tmp_path / "four.json", wire=_MISSING)
    result = _run_agent(daimonos_binary, tmp_path, "--model-profile", str(profile))
    assert result.returncode != 0
    combined = result.stderr + result.stdout
    assert "missing" in combined and "wire" in combined, combined
    assert "base_url" not in combined, combined


def test_wire_is_not_inferred_and_invalid_wire_is_refused(daimonos_binary, tmp_path):
    # FR-04 req 3: the wire is never inferred; an invalid value is refused by name.
    profile = _write_profile(tmp_path / "badwire.json", wire="guess")
    result = _run_agent(daimonos_binary, tmp_path, "--model-profile", str(profile))
    assert result.returncode != 0
    assert "wire" in (result.stderr + result.stdout)


def test_unconfigured_extra_field_is_refused(daimonos_binary, tmp_path):
    # FR-04 req 6 default: a field beyond the known set fails unless explicitly
    # configured (sampling is never an explicit controlled field).
    path = tmp_path / "extra.json"
    data = json.loads(_write_profile(tmp_path / "base.json").read_text())
    data["temperature"] = 0.7
    path.write_text(json.dumps(data))
    result = _run_agent(daimonos_binary, tmp_path, "--model-profile", str(path))
    assert result.returncode != 0
    assert "unconfigured" in (result.stderr + result.stdout)


def test_profile_cannot_be_combined_with_model_or_provider_flags(daimonos_binary, tmp_path):
    # FR-04 req 2: values must not be combined from different sources. The flag
    # is mutually exclusive with --model/--provider at the CLI boundary.
    profile = _write_profile(tmp_path / "full.json")
    result = _run_agent(
        daimonos_binary, tmp_path, "--model-profile", str(profile), "--model", "other"
    )
    assert result.returncode != 0
    assert "model-profile" in (result.stderr + result.stdout)


def test_complete_profile_is_accepted(daimonos_binary, tmp_path):
    # A complete profile validates at startup; dry-run then proceeds with no
    # request, proving the five values plus request settings parse.
    profile = _write_profile(
        tmp_path / "full.json",
        wire="chat",
        provider_label="openrouter",
        model="some/model",
        base_url="http://127.0.0.1:1",
        max_tokens=4096,
        timeout_ms=30000,
        request_retries=0,
    )
    result = _run_agent(daimonos_binary, tmp_path, "--model-profile", str(profile))
    assert result.returncode == 0, result.stderr + result.stdout


def test_default_off_is_the_legacy_dry_run(daimonos_binary, tmp_path):
    # Opt-out regression: with no --model-profile the agent behaves exactly as
    # before (a plain dry-run succeeds and prints the tool/task preview).
    result = _run_agent(daimonos_binary, tmp_path)
    assert result.returncode == 0, result.stderr + result.stdout
