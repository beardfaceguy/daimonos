# Graph Report - daimonos  (2026-05-27)

## Corpus Check
- 111 files · ~85,543 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 1709 nodes · 2737 edges · 108 communities (94 shown, 14 thin omitted)
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS · INFERRED: 5 edges (avg confidence: 0.8)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `8e014d0e`
- Run `git rev-parse HEAD` and compare to check if the graph is stale.
- Run `graphify update .` after code changes (no API cost).

## Community Hubs (Navigation)
- [[_COMMUNITY_Community 0|Community 0]]
- [[_COMMUNITY_Community 1|Community 1]]
- [[_COMMUNITY_Community 2|Community 2]]
- [[_COMMUNITY_Community 3|Community 3]]
- [[_COMMUNITY_Community 4|Community 4]]
- [[_COMMUNITY_Community 5|Community 5]]
- [[_COMMUNITY_Community 6|Community 6]]
- [[_COMMUNITY_Community 7|Community 7]]
- [[_COMMUNITY_Community 8|Community 8]]
- [[_COMMUNITY_Community 9|Community 9]]
- [[_COMMUNITY_Community 10|Community 10]]
- [[_COMMUNITY_Community 11|Community 11]]
- [[_COMMUNITY_Community 12|Community 12]]
- [[_COMMUNITY_Community 13|Community 13]]
- [[_COMMUNITY_Community 14|Community 14]]
- [[_COMMUNITY_Community 15|Community 15]]
- [[_COMMUNITY_Community 16|Community 16]]
- [[_COMMUNITY_Community 17|Community 17]]
- [[_COMMUNITY_Community 18|Community 18]]
- [[_COMMUNITY_Community 19|Community 19]]
- [[_COMMUNITY_Community 20|Community 20]]
- [[_COMMUNITY_Community 21|Community 21]]
- [[_COMMUNITY_Community 22|Community 22]]
- [[_COMMUNITY_Community 23|Community 23]]
- [[_COMMUNITY_Community 24|Community 24]]
- [[_COMMUNITY_Community 25|Community 25]]
- [[_COMMUNITY_Community 26|Community 26]]
- [[_COMMUNITY_Community 27|Community 27]]
- [[_COMMUNITY_Community 28|Community 28]]
- [[_COMMUNITY_Community 29|Community 29]]
- [[_COMMUNITY_Community 30|Community 30]]
- [[_COMMUNITY_Community 31|Community 31]]
- [[_COMMUNITY_Community 32|Community 32]]
- [[_COMMUNITY_Community 33|Community 33]]
- [[_COMMUNITY_Community 34|Community 34]]
- [[_COMMUNITY_Community 35|Community 35]]
- [[_COMMUNITY_Community 36|Community 36]]
- [[_COMMUNITY_Community 37|Community 37]]
- [[_COMMUNITY_Community 38|Community 38]]
- [[_COMMUNITY_Community 39|Community 39]]
- [[_COMMUNITY_Community 40|Community 40]]
- [[_COMMUNITY_Community 41|Community 41]]
- [[_COMMUNITY_Community 42|Community 42]]
- [[_COMMUNITY_Community 43|Community 43]]
- [[_COMMUNITY_Community 44|Community 44]]
- [[_COMMUNITY_Community 45|Community 45]]
- [[_COMMUNITY_Community 46|Community 46]]
- [[_COMMUNITY_Community 47|Community 47]]
- [[_COMMUNITY_Community 48|Community 48]]
- [[_COMMUNITY_Community 49|Community 49]]
- [[_COMMUNITY_Community 50|Community 50]]
- [[_COMMUNITY_Community 51|Community 51]]
- [[_COMMUNITY_Community 52|Community 52]]
- [[_COMMUNITY_Community 53|Community 53]]
- [[_COMMUNITY_Community 54|Community 54]]
- [[_COMMUNITY_Community 55|Community 55]]
- [[_COMMUNITY_Community 56|Community 56]]
- [[_COMMUNITY_Community 57|Community 57]]
- [[_COMMUNITY_Community 58|Community 58]]
- [[_COMMUNITY_Community 59|Community 59]]
- [[_COMMUNITY_Community 60|Community 60]]
- [[_COMMUNITY_Community 61|Community 61]]
- [[_COMMUNITY_Community 62|Community 62]]
- [[_COMMUNITY_Community 63|Community 63]]
- [[_COMMUNITY_Community 64|Community 64]]
- [[_COMMUNITY_Community 65|Community 65]]
- [[_COMMUNITY_Community 66|Community 66]]
- [[_COMMUNITY_Community 67|Community 67]]
- [[_COMMUNITY_Community 68|Community 68]]
- [[_COMMUNITY_Community 69|Community 69]]
- [[_COMMUNITY_Community 70|Community 70]]
- [[_COMMUNITY_Community 71|Community 71]]
- [[_COMMUNITY_Community 72|Community 72]]
- [[_COMMUNITY_Community 73|Community 73]]
- [[_COMMUNITY_Community 74|Community 74]]
- [[_COMMUNITY_Community 75|Community 75]]
- [[_COMMUNITY_Community 76|Community 76]]
- [[_COMMUNITY_Community 77|Community 77]]
- [[_COMMUNITY_Community 78|Community 78]]
- [[_COMMUNITY_Community 79|Community 79]]
- [[_COMMUNITY_Community 80|Community 80]]
- [[_COMMUNITY_Community 81|Community 81]]
- [[_COMMUNITY_Community 82|Community 82]]
- [[_COMMUNITY_Community 83|Community 83]]
- [[_COMMUNITY_Community 84|Community 84]]
- [[_COMMUNITY_Community 85|Community 85]]
- [[_COMMUNITY_Community 86|Community 86]]
- [[_COMMUNITY_Community 87|Community 87]]
- [[_COMMUNITY_Community 88|Community 88]]
- [[_COMMUNITY_Community 89|Community 89]]
- [[_COMMUNITY_Community 90|Community 90]]
- [[_COMMUNITY_Community 91|Community 91]]
- [[_COMMUNITY_Community 92|Community 92]]
- [[_COMMUNITY_Community 93|Community 93]]
- [[_COMMUNITY_Community 94|Community 94]]
- [[_COMMUNITY_Community 95|Community 95]]
- [[_COMMUNITY_Community 96|Community 96]]
- [[_COMMUNITY_Community 97|Community 97]]
- [[_COMMUNITY_Community 98|Community 98]]
- [[_COMMUNITY_Community 99|Community 99]]
- [[_COMMUNITY_Community 100|Community 100]]
- [[_COMMUNITY_Community 101|Community 101]]
- [[_COMMUNITY_Community 102|Community 102]]
- [[_COMMUNITY_Community 103|Community 103]]
- [[_COMMUNITY_Community 104|Community 104]]
- [[_COMMUNITY_Community 105|Community 105]]

## God Nodes (most connected - your core abstractions)
1. `session_in()` - 44 edges
2. `write()` - 39 edges
3. `exec()` - 25 edges
4. `session_in()` - 19 edges
5. `execute()` - 18 edges
6. `setup_git_repo()` - 18 edges
7. `read()` - 16 edges
8. `test_store()` - 15 edges
9. `_parse()` - 15 edges
10. `AnalyticsStore` - 14 edges

## Surprising Connections (you probably didn't know these)
- `dispatch_tool_inner()` --calls--> `get_str()`  [INFERRED]
  src/mcp.rs → src/tools.rs
- `test_initialize_instructions_contain_workspace_context()` --calls--> `DaimonosClient`  [INFERRED]
  tests/test_handshake.py → tests/conftest.py
- `test_initialize_returns_server_info()` --calls--> `DaimonosClient`  [INFERRED]
  tests/test_handshake.py → tests/conftest.py

## Communities (108 total, 14 thin omitted)

### Community 0 - "Community 0"
Cohesion: 0.13
Nodes (55): glob(), glob_finds_files(), glob_includes_symlinks(), glob_match_simple(), grep(), grep_blocking(), grep_finds_matches(), grep_through_symlink() (+47 more)

### Community 1 - "Community 1"
Cohesion: 0.10
Nodes (50): bg(), bg_log_files_cleaned_up_after_completion(), bg_passes_op_kv_to_subprocess(), bg_poll_kill_lifecycle(), bg_process_removed_after_completion(), bg_process_removed_after_kill(), bg_processes_dont_accumulate(), build_command() (+42 more)

### Community 2 - "Community 2"
Cohesion: 0.08
Nodes (41): build_globals(), cancel_on_drop_sets_flag_when_dropped(), CancelOnDrop, configure_max_concurrent(), dispatch_request(), dispatch_tool_by_name(), execute(), execute_captures_print() (+33 more)

### Community 3 - "Community 3"
Cohesion: 0.04
Nodes (44): 10. SysRq — `CONFIG_MAGIC_SYSRQ=y`, 1. NVMe Storage — `CONFIG_BLK_DEV_NVME=y`, 2. ENA Network Driver — `CONFIG_ENA_ETHERNET=y`, 3. PCI MSI/MSI-X Interrupts — `CONFIG_PCI_MSI=y`, 4. ACPI Hardware Discovery — `CONFIG_ACPI=y`, 5. VGA Console — `CONFIG_VGA_CONSOLE=y`, 6. Serial Console — `CONFIG_SERIAL_8250=y` + `CONFIG_SERIAL_8250_CONSOLE=y`, 7. Hypervisor Guest Support (+36 more)

### Community 4 - "Community 4"
Cohesion: 0.11
Nodes (30): descriptor_serialization_roundtrip(), echo_descriptor(), extract_json_patch_fixes(), extract_json_patch_fixes_empty(), extract_json_patch_fixes_valid(), extract_string_replace_fixes(), extract_string_replace_fixes_empty(), extract_string_replace_fixes_valid() (+22 more)

### Community 5 - "Community 5"
Cohesion: 0.07
Nodes (43): _create_rust_project(), _is_error(), _parse(), End-to-end MCP integration tests for cargo, gh, and docker plugins., cargo build returns structured output., cargo fmt --check returns formatting status., cargo clippy returns structured diagnostics., cargo tool appears in tool listing when Cargo.toml exists. (+35 more)

### Community 6 - "Community 6"
Cohesion: 0.13
Nodes (24): AnalyticsStore, daily_trend_groups_by_date(), DailyStats, empty_db_returns_zeros(), format_stats_report_not_empty(), history_summary_recovers_from_poisoned_db_mutex(), history_summary_returns_data(), HistorySummary (+16 more)

### Community 7 - "Community 7"
Cohesion: 0.17
Nodes (32): git_add(), git_branch(), git_checkout(), git_commit(), git_diff(), git_log(), git_pull(), git_push() (+24 more)

### Community 8 - "Community 8"
Cohesion: 0.09
Nodes (24): ansi_re(), build_filter_failure(), build_filter_success_no_warnings(), build_filter_success_with_warnings(), classify(), error_line_re(), ExecFilter, filter_build_output() (+16 more)

### Community 9 - "Community 9"
Cohesion: 0.06
Nodes (35): author, name, url, compatibility, platforms, description, display_name, documentation (+27 more)

### Community 10 - "Community 10"
Cohesion: 0.06
Nodes (34): 60-second demo, Additional capabilities, Architecture, Benchmark results, code:block1 (Agent: exec("cargo test")), code:bash (# 1) Install daimonos), code:bash (# Linux x86_64), code:bash (git clone https://github.com/beardfaceguy/daimonos.git) (+26 more)

### Community 11 - "Community 11"
Cohesion: 0.13
Nodes (24): CacheEntry, content_hash_deterministic(), decl_cache_bounded_after_many_entries(), decl_cache_failing_lint(), decl_cache_miss_then_hit(), DeclCache, extract_quickfixes_empty_when_no_diagnostics(), extract_quickfixes_from_diagnostics() (+16 more)

### Community 12 - "Community 12"
Cohesion: 0.11
Nodes (21): activate_all_tools_adds_on_demand(), activate_tool_adds_custom_to_exposed(), alloc_pid_monotonic(), BgProcess, collect_tool_dirs(), enhance_process_path(), exec_usage_bounded_after_many_commands(), exposed_tools_includes_tier0_and_tier1() (+13 more)

### Community 13 - "Community 13"
Cohesion: 0.17
Nodes (28): chrono_now(), clean_empty_dirs(), collect_relative_paths(), collect_workspace_paths(), copy_workspace(), create_impl(), create_snapshot(), create_without_tag() (+20 more)

### Community 14 - "Community 14"
Cohesion: 0.06
Nodes (32): 1. Create the MCP config file, 2. Run with daimonos, 3. Convenience alias (optional), Benchmarking, Biasing the model toward daimonos in the Desktop app, Claude Code Setup, CLI flags reference, CLI workflow (+24 more)

### Community 15 - "Community 15"
Cohesion: 0.20
Nodes (23): concurrent_reindexes_serialize_correctly(), extract_trigrams(), extract_trigrams_deduplicates_per_file(), extract_trigrams_from_content(), extract_trigrams_short_content(), incremental_adds_new_files(), incremental_combined_add_delete_modify(), incremental_removes_deleted_files() (+15 more)

### Community 16 - "Community 16"
Cohesion: 0.17
Nodes (20): cache_bounded_after_many_entries(), cache_evicts_least_recently_used_entry(), cache_max_entries_is_configurable(), cache_miss_on_empty(), cache_put_refreshes_recency(), CachedResult, CacheState, count_inotify_watches() (+12 more)

### Community 17 - "Community 17"
Cohesion: 0.15
Nodes (20): build_instructions(), build_instructions_detects_cargo(), build_instructions_detects_git(), build_instructions_includes_workspace(), build_instructions_lists_dirs(), DaimonosHandler, dispatch_tool(), dispatch_tool_inner() (+12 more)

### Community 18 - "Community 18"
Cohesion: 0.18
Nodes (23): append_package_arg(), cargo_add(), cargo_diagnostics(), cargo_fmt(), cargo_test(), CargoPlugin, extract_span_location(), extract_span_location_no_spans() (+15 more)

### Community 19 - "Community 19"
Cohesion: 0.18
Nodes (24): descriptor_has_all_commands(), docker_compose_down(), docker_compose_ps(), docker_compose_up(), docker_exec(), docker_images(), docker_inspect(), docker_logs() (+16 more)

### Community 20 - "Community 20"
Cohesion: 0.10
Nodes (16): AnalyticsConfig, Config, default_config_values(), default_skip_extensions(), dirs_next(), IndexConfig, load(), load_falls_back_to_defaults() (+8 more)

### Community 21 - "Community 21"
Cohesion: 0.11
Nodes (19): all_tool_names(), all_tools(), all_tools_has_entries(), all_tools_have_descriptions(), all_tools_no_duplicates(), build_request(), build_request_for_opcode_tools(), get_str() (+11 more)

### Community 22 - "Community 22"
Cohesion: 0.10
Nodes (21): daimonos(), daimonos_binary(), DaimonosClient, _find_binary(), str, Manages a daimonos subprocess and sends JSON-RPC over stdio., Send a JSON-RPC message. Returns the response, or None for notifications., Return path to daimonos binary, building if necessary. (+13 more)

### Community 23 - "Community 23"
Cohesion: 0.07
Nodes (21): Tests for exec MCP tool., Successful cargo build via exec should return structured output (plugin redirect, cargo test via exec should return compact output (plugin redirect or filter)., Unknown commands should pass through unfiltered., exec('cargo test') should redirect through native cargo plugin., exec('git status') should redirect through native git plugin., Commands that don't match a plugin should go through raw exec., Verify PATH includes auto-detected tool directories. (+13 more)

### Community 24 - "Community 24"
Cohesion: 0.07
Nodes (21): Tests for read_file and write_file MCP tools., Paginated reads always return content, never 'unchanged'., Full read of a file ending in '\\n' must return content ending in '\\n'., Full read of a file WITHOUT trailing newline must not gain one., The canonical regression for vikunja #246: read then write must be     byte-iden, Offset read that reaches EOF must keep the file's trailing newline., Limited read that does NOT reach EOF must not append a newline., Limited read whose slice happens to end at EOF must keep the newline. (+13 more)

### Community 25 - "Community 25"
Cohesion: 0.12
Nodes (21): int, Path, str, bool, CallSample, Client, find_binary(), load_task() (+13 more)

### Community 26 - "Community 26"
Cohesion: 0.18
Nodes (24): build_tool_args(), build_tool_args_empty(), build_tool_args_with_fields(), build_tool_args_with_kv(), test_session_no_registry(), test_session_with_registry(), tool_list(), tool_list_no_registry() (+16 more)

### Community 27 - "Community 27"
Cohesion: 0.09
Nodes (21): 1. Create the MCP config file, 2. Start the server, 3. Use in Copilot Chat, code:json ({), code:json ({), code:json ({), code:json ({), code:json ({) (+13 more)

### Community 28 - "Community 28"
Cohesion: 0.09
Nodes (21): Tests for the batch MCP tool., Nested batch calls are rejected., Batch without ops field returns error., Batch can run exec commands., Batch can mix different tool types., Batch continues on failure and reports per-op status., Batch with empty ops array returns empty results., Batch ops without 'tool' field produce errors. (+13 more)

### Community 29 - "Community 29"
Cohesion: 0.25
Nodes (20): _git(), _init_repo(), _parse(), Tests for the unified git MCP tool., Unified git tool is visible when workspace has .git., Extended tools like diff_files, tool_pipeline are not in initial listing., test_git_branch_current(), test_git_branch_multiple() (+12 more)

### Community 30 - "Community 30"
Cohesion: 0.10
Nodes (20): 1. Create the MCP config file, 2. Verify in Cursor, 3. Add the system prompt directive (recommended), Agent still uses built-in tools, code:json ({), code:json ({), code:json ({), code:block4 (---) (+12 more)

### Community 31 - "Community 31"
Cohesion: 0.17
Nodes (10): Op, Request, Response, response_err_serialization(), response_meta_builders_compose(), response_meta_builders_set_individual_flags(), response_meta_defaults_all_false(), response_meta_is_skipped_in_wire_format() (+2 more)

### Community 32 - "Community 32"
Cohesion: 0.11
Nodes (18): code:bash (# 1. Build daimonos and set up MCP config for the benchmark ), code:block2 (benchmarks/), code:bash (./run-benchmark.sh cursor 03   # runs only task 03-edit-rena), code:bash (# Build the modified distro (includes Node.js + bench user)), code:bash (# Keep instances alive for debugging), Daimonos Token Usage Benchmark, Environment variables, Environment variables (+10 more)

### Community 33 - "Community 33"
Cohesion: 0.11
Nodes (17): code:block1 (daimonos/), code:bash (cargo test), code:bash (# Install deps (one time)), code:bash (# --- MCP mode (stdio, for Cursor integration) ---), Coding conventions, Daimonos tool usage policy, Layer 1: Rust unit/integration tests, Layer 2: pytest MCP protocol conformance (+9 more)

### Community 34 - "Community 34"
Cohesion: 0.11
Nodes (17): code:bash (cp daimonos.default.toml ~/.config/daimonos/config.toml), code:bash (cp daimonos.default.toml /path/to/project/daimonos.toml), code:toml ([index]), code:toml ([search]), code:toml ([process]), code:toml ([mcp]), code:toml ([tools.x07]), Config File Location (+9 more)

### Community 35 - "Community 35"
Cohesion: 0.11
Nodes (17): Android Studio (Gemini Agent), AnythingLLM, BoltAI, ChatGPT Desktop, Claude Desktop, code:json ({), code:json ({), code:json ({) (+9 more)

### Community 36 - "Community 36"
Cohesion: 0.11
Nodes (17): Batch Operations, code:block1 ([op: u8, ...args]), code:block2 ([0, "src/main.rs", 10, 50]), code:json ({"ok": true, "d": <result data>}), code:json ({"ok": false, "e": <error code>, "m": <message>}), code:json ({"batch": [), Compact Mode (optional, non-MCP clients), Daimonos Protocol Specification v0.1 (+9 more)

### Community 37 - "Community 37"
Cohesion: 0.11
Nodes (17): Tests for token analytics and session_stats tool (CLA-297)., session_stats should appear in the default tool list (Terse tier)., History scope queries SQLite for cross-session data., Daily scope returns trend data., Invalid scope returns an error., workspace_info should include an analytics summary after tool calls., After a few tool calls, session_stats should report non-zero totals., Reading the same file twice should trigger a dedup hit in analytics. (+9 more)

### Community 38 - "Community 38"
Cohesion: 0.27
Nodes (13): descriptor_has_all_commands(), gh_api(), gh_pr_checks(), gh_pr_create(), gh_pr_diff(), gh_pr_list(), gh_pr_view(), GhPlugin (+5 more)

### Community 39 - "Community 39"
Cohesion: 0.16
Nodes (15): _get_rss_kb(), _parse(), int, str, Memory regression tests.  These verify that daimonos does not leak memory under, A sustained mixed workload should not show monotonic RSS growth.      This is th, Get resident set size in KB for a process., Extract text content from MCP tool result. (+7 more)

### Community 40 - "Community 40"
Cohesion: 0.13
Nodes (14): code:bash (# x86_64 (most desktops and servers)), code:bash (# Apple Silicon (M1/M2/M3/M4)), code:bash (daimonos --help), code:bash (# 1. Clone the repository), code:json ({), Configure Your IDE, Daimonos — Installation & Setup, Further Reading (+6 more)

### Community 41 - "Community 41"
Cohesion: 0.21
Nodes (12): _parse(), Tests for the unified snapshot MCP tool., Unified snapshot tool is visible in the initial tool listing., test_multiple_snapshots_independent(), test_snapshot_create(), test_snapshot_create_without_tag(), test_snapshot_delete(), test_snapshot_list_empty() (+4 more)

### Community 42 - "Community 42"
Cohesion: 0.13
Nodes (3): Tests for symbolic link and hard link handling across all file operations., Broken symlinks should not crash workspace_info., test_stat_broken_symlink_via_workspace()

### Community 43 - "Community 43"
Cohesion: 0.13
Nodes (5): Integration tests for tool_pipeline and tool_repair MCP tools.  These tools oper, Tests for the tool_pipeline MCP tool., Tests for the tool_repair MCP tool., TestToolPipeline, TestToolRepair

### Community 44 - "Community 44"
Cohesion: 0.15
Nodes (12): Avoid: Conditional environment inheritance, Avoid: Insert-only maps without remove, Avoid: Testing only the happy path, code:rust (// Encapsulate the bound check in a single method), code:rust (match proc.try_wait() {), code:rust (let dirty = Arc::new(AtomicBool::new(false));), code:rust (// WRONG: PATH missing when no extras), code:rust (// INCOMPLETE: only tests creation) (+4 more)

### Community 45 - "Community 45"
Cohesion: 0.38
Nodes (12): restore(), restore_missing_id(), restore_nonexistent(), session_in(), snap(), snap_and_restore_roundtrip(), snap_delete(), snap_delete_existing() (+4 more)

### Community 46 - "Community 46"
Cohesion: 0.17
Nodes (11): Build Notes, code:bash (# x86_64 (most Ubuntu/Debian desktops and servers)), code:bash (# Apple Silicon (M1/M2/M3/M4)), code:bash (# 1. Clone the repo), code:block4 (Daimonos — agent-optimized OS layer), Installing Daimonos, Next Steps, Option A: Download Pre-Built Binary (+3 more)

### Community 47 - "Community 47"
Cohesion: 0.29
Nodes (11): Popen, _handshake(), int, str, Process lifecycle tests for daimonos --mcp.  Bug being prevented: a daimonos --m, The leak scenario: parent stays alive (stdin write-end stays open)     but never, Tool calls reset the idle clock so an active session is never killed., _send() (+3 more)

### Community 48 - "Community 48"
Cohesion: 0.21
Nodes (11): _err_text(), str, Regression tests for set_cwd (vikunja #249, fix #7).  The bug: `set_cwd` checked, Non-existent path must produce a canonicalize/resolve error,     not a 'not a di, When set_cwd is given a path to a regular file, the error must     reference the, When set_cwd is given a symlink pointing at a file, the error must     reference, Sanity check: setting cwd to a real subdirectory should succeed     and report t, test_set_cwd_missing_path_returns_canonicalize_error() (+3 more)

### Community 49 - "Community 49"
Cohesion: 0.18
Nodes (10): 1. Open MCP settings, 2. Add daimonos, 3. Verify, Cline Setup, code:json ({), code:block2 (Use daimonos MCP tools for all file, search, exec, and git o), Custom Instructions (optional), Prerequisites (+2 more)

### Community 50 - "Community 50"
Cohesion: 0.18
Nodes (10): 1. Edit Gemini CLI settings, 2. Run Gemini CLI, 3. Verify, code:json ({), code:json ({), code:bash (gemini), Gemini CLI Setup, Prerequisites (+2 more)

### Community 51 - "Community 51"
Cohesion: 0.18
Nodes (10): 1. Open MCP configuration, 2. Add daimonos as an MCP server, 3. Verify, Adding a Rules Directive (optional), code:json ({), code:block2 (Use daimonos MCP tools for all file, search, exec, and git o), Prerequisites, Setup (+2 more)

### Community 52 - "Community 52"
Cohesion: 0.31
Nodes (10): int, Path, str, float, compare_task(), fmt_ns(), fmt_pct(), load_results() (+2 more)

### Community 53 - "Community 53"
Cohesion: 0.18
Nodes (10): Adding a task, code:bash (cargo build --release   # if you haven't already), code:bash (python3 benchmarks/server-bench/compare.py \), code:json ({), Output schema, Quick start, server-bench, Tasks (+2 more)

### Community 54 - "Community 54"
Cohesion: 0.18
Nodes (10): code:rust (#[test]), code:rust (#[tokio::test]), code:rust (#[tokio::test]), code:python (def test_memory_stable_under_load(daimonos):), Rust Testing Strategies for Resource Management, Strategy 1: Bounded growth tests, Strategy 2: Full lifecycle tests, Strategy 3: Accumulation stress tests (+2 more)

### Community 55 - "Community 55"
Cohesion: 0.18
Nodes (7): Tests for error handling: missing args, unknown tools, invalid paths., Edits array must have even length (old/new pairs)., Absolute path outside workspace — should still work (no jail) but returns valid, Calling a nonexistent tool should signal an error., test_edit_file_odd_edits(), test_read_outside_workspace(), test_unknown_tool_returns_error()

### Community 56 - "Community 56"
Cohesion: 0.18
Nodes (7): Tests for search MCP tool (content grep and file name search)., workspace_info should report index stats including file count., Search by filename uses the trigram index. Index needs a moment to build., After writing a new file and waiting for reindex, it should be searchable., test_file_search_via_trigram(), test_incremental_index_picks_up_new_files(), test_index_stats_in_workspace_info()

### Community 57 - "Community 57"
Cohesion: 0.36
Nodes (9): find_runs(), latest_run(), load_run(), main(), print_comparison(), Path, str, Load all task results from a run directory. (+1 more)

### Community 58 - "Community 58"
Cohesion: 0.20
Nodes (9): 1. Open Zed settings, 2. Add daimonos as an MCP server, 3. Verify, code:json ({), code:json ({), Prerequisites, Setup, Troubleshooting (+1 more)

### Community 59 - "Community 59"
Cohesion: 0.33
Nodes (7): run-remote-benchmark.sh script, collect_results(), provision_instance(), run_on_instance(), ssh_upload_dir(), ssh_upload_file(), wait_ssh()

### Community 60 - "Community 60"
Cohesion: 0.20
Nodes (9): description, name, packages, repository, source, url, $schema, title (+1 more)

### Community 61 - "Community 61"
Cohesion: 0.20
Nodes (9): 1. HashMap/Vec in long-lived structs, 2. Temp files and child processes, 3. Callbacks and closures, 4. Shared state (`Arc<Mutex<T>>` / `Arc<RwLock<T>>`), 5. Environment and configuration, code:bash (# Find all HashMap/Vec fields in long-lived structs), Pre-commit checklist for new code, Quick audit commands (+1 more)

### Community 62 - "Community 62"
Cohesion: 0.20
Nodes (5): Tests for edit_file MCP tool., edit_file should return a diffs array confirming each applied change., When no edits match, diffs should be absent., test_edit_no_diffs_when_nothing_matches(), test_edit_returns_diffs()

### Community 63 - "Community 63"
Cohesion: 0.22
Nodes (8): Before opening a pull request, code:bash (cargo build), code:bash (cargo test), code:bash (python3 -m pytest tests/ -v), Contributing, Development setup, Project context for coding agents, Pull request guidelines

### Community 64 - "Community 64"
Cohesion: 0.22
Nodes (8): Breaking changes, Highlights, Installation, Known issues, Release notes, Summary, Upgrade notes, Verification

### Community 65 - "Community 65"
Cohesion: 0.39
Nodes (8): each_op_has_required_fields(), full_registry_returns_all_ops(), known_opcodes_present(), op_schema(), op_schema_helper_required_params(), schema(), specific_op_returns_single(), unknown_specific_op_returns_error()

### Community 66 - "Community 66"
Cohesion: 0.33
Nodes (6): _parse(), Tests for the diff_files MCP tool., test_diff_different_files(), test_diff_file_vs_content(), test_diff_hunk_line_ranges(), test_diff_identical_files()

### Community 67 - "Community 67"
Cohesion: 0.25
Nodes (7): applies_to, category, expected_behavior, id, name, prompt, success_criteria

### Community 68 - "Community 68"
Cohesion: 0.25
Nodes (7): applies_to, category, expected_behavior, id, name, prompt, success_criteria

### Community 69 - "Community 69"
Cohesion: 0.25
Nodes (7): applies_to, category, expected_behavior, id, name, prompt, success_criteria

### Community 70 - "Community 70"
Cohesion: 0.25
Nodes (7): applies_to, category, expected_behavior, id, name, prompt, success_criteria

### Community 71 - "Community 71"
Cohesion: 0.25
Nodes (7): applies_to, category, expected_behavior, id, name, prompt, success_criteria

### Community 72 - "Community 72"
Cohesion: 0.25
Nodes (7): applies_to, category, expected_behavior, id, name, prompt, success_criteria

### Community 73 - "Community 73"
Cohesion: 0.25
Nodes (7): applies_to, category, expected_behavior, id, name, prompt, success_criteria

### Community 74 - "Community 74"
Cohesion: 0.25
Nodes (7): applies_to, category, expected_behavior, id, name, prompt, success_criteria

### Community 75 - "Community 75"
Cohesion: 0.25
Nodes (7): applies_to, category, expected_behavior, id, name, prompt, success_criteria

### Community 76 - "Community 76"
Cohesion: 0.25
Nodes (7): applies_to, category, expected_behavior, id, name, prompt, success_criteria

### Community 77 - "Community 77"
Cohesion: 0.61
Nodes (7): diff(), diff_different_files(), diff_file_vs_content(), diff_identical_files(), diff_missing_args(), diff_missing_file(), session_in()

### Community 78 - "Community 78"
Cohesion: 0.54
Nodes (5): descriptor_round_trip(), GenericCliPlugin, make_descriptor(), new_stores_descriptor(), no_quickfixes_by_default()

### Community 79 - "Community 79"
Cohesion: 0.25
Nodes (7): applies_to, category, expected_behavior, id, name, prompt, success_criteria

### Community 80 - "Community 80"
Cohesion: 0.48
Nodes (5): check_response(), die(), fail(), pass(), smoke-test.sh script

### Community 81 - "Community 81"
Cohesion: 0.52
Nodes (6): dispatch(), dispatch_op(), env_get(), env_set(), find(), session_info()

### Community 82 - "Community 82"
Cohesion: 0.48
Nodes (6): Cli, env_requests_mcp_startup_logs(), handle_connection(), install_parent_death_signal(), main(), run_socket_server()

### Community 83 - "Community 83"
Cohesion: 0.60
Nodes (5): find_tagged_run(), load_run(), main(), Path, str

### Community 84 - "Community 84"
Cohesion: 0.47
Nodes (3): reset_workspace(), run_task(), run-benchmark.sh script

### Community 85 - "Community 85"
Cohesion: 0.50
Nodes (4): Path, Run 30 cheap exec calls (`true`) to stress process-spawn overhead.  Why `true` a, run_iteration(), setup()

### Community 86 - "Community 86"
Cohesion: 0.40
Nodes (4): Added, Changed, Changelog, [Unreleased]

### Community 87 - "Community 87"
Cohesion: 0.40
Nodes (4): Reporting a vulnerability, Response expectations, Security Policy, Supported versions

### Community 88 - "Community 88"
Cohesion: 0.50
Nodes (4): Path, Read 100 small files sequentially.  Exercises the file-IO opcode path: dispatch, run_iteration(), setup()

### Community 89 - "Community 89"
Cohesion: 0.50
Nodes (4): Path, Run 50 grep calls with varied patterns against a synthetic source tree.  Stresse, run_iteration(), setup()

### Community 90 - "Community 90"
Cohesion: 0.50
Nodes (4): Path, Snapshot create + restore + delete cycle, repeated N times per iteration.  Stres, run_iteration(), setup()

### Community 92 - "Community 92"
Cohesion: 0.50
Nodes (3): CI smoke test for the deterministic server-bench harness.  Runs a single task at, End-to-end: bench.py spawns daimonos, runs read_100 × 2 replicates,     writes a, test_bench_harness_runs_one_task()

## Knowledge Gaps
- **405 isolated node(s):** `$schema`, `name`, `title`, `description`, `version` (+400 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **14 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `CancelOnDrop` connect `Community 2` to `Community 16`?**
  _High betweenness centrality (0.002) - this node is a cross-community bridge._
- **What connects `$schema`, `name`, `title` to the rest of the system?**
  _533 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Community 0` be split into smaller, more focused modules?**
  _Cohesion score 0.12597402597402596 - nodes in this community are weakly interconnected._
- **Should `Community 1` be split into smaller, more focused modules?**
  _Cohesion score 0.09869375907111756 - nodes in this community are weakly interconnected._
- **Should `Community 2` be split into smaller, more focused modules?**
  _Cohesion score 0.07712765957446809 - nodes in this community are weakly interconnected._
- **Should `Community 3` be split into smaller, more focused modules?**
  _Cohesion score 0.044444444444444446 - nodes in this community are weakly interconnected._
- **Should `Community 4` be split into smaller, more focused modules?**
  _Cohesion score 0.10993657505285412 - nodes in this community are weakly interconnected._