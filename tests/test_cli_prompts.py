"""CLI smoke tests for the baseline-prompt commands (vikunja #980):
`--print-prompt` and `--dump-prompts`."""

import subprocess

PROMPT_NAMES = ["agent_system", "mcp_instructions", "kgl_hint", "summary"]


def _run(binary, args, cwd=None):
    return subprocess.run(
        [binary, *args], capture_output=True, text=True, cwd=cwd, timeout=30
    )


def test_print_prompt_emits_default(daimonos_binary):
    r = _run(daimonos_binary, ["--print-prompt", "mcp_instructions"])
    assert r.returncode == 0
    assert "Terse output" in r.stdout
    assert "Use daimonos tools" in r.stdout


def test_print_prompt_unknown_name_errors(daimonos_binary):
    r = _run(daimonos_binary, ["--print-prompt", "not_a_prompt"])
    assert r.returncode == 2
    assert "unknown prompt" in r.stderr
    # Lists the valid names so the user can recover.
    for name in PROMPT_NAMES:
        assert name in r.stderr


def test_dump_prompts_scaffolds_all_files(daimonos_binary, tmp_path):
    target = tmp_path / "prompts"
    r = _run(daimonos_binary, ["--dump-prompts", str(target)])
    assert r.returncode == 0
    for name in PROMPT_NAMES:
        f = target / f"{name}.md"
        assert f.is_file(), f"missing {name}.md"
        assert f.read_text(encoding="utf-8").strip()
    # Prints a ready-to-paste config block.
    assert "[prompts]" in r.stdout


def test_dump_prompts_skips_existing_without_force(daimonos_binary, tmp_path):
    target = tmp_path / "prompts"
    target.mkdir()
    edited = target / "summary.md"
    edited.write_text("USER EDITED", encoding="utf-8")

    r = _run(daimonos_binary, ["--dump-prompts", str(target)])
    assert r.returncode == 0
    assert "skipped" in r.stdout
    # The user's file is preserved.
    assert edited.read_text(encoding="utf-8") == "USER EDITED"

    # --force overwrites it with the baseline.
    r2 = _run(daimonos_binary, ["--dump-prompts", str(target), "--force"])
    assert r2.returncode == 0
    assert edited.read_text(encoding="utf-8") != "USER EDITED"


def test_force_requires_dump_prompts(daimonos_binary):
    r = _run(daimonos_binary, ["--force"])
    assert r.returncode == 2
    assert "--dump-prompts" in r.stderr


def test_agent_instructions_flag_reports_unreadable_explicit_file(
    daimonos_binary, tmp_path
):
    missing = tmp_path / "missing-agent-instructions.md"
    r = _run(
        daimonos_binary,
        ["acp", "--agent-instructions", str(missing)],
    )
    assert r.returncode == 2
    assert "agent instructions" in r.stderr
    assert str(missing) in r.stderr
