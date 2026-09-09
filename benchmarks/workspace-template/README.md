# Inventory benchmark fixture

A deliberately small Rust application used by the Daimonos agent benchmark.
It loads `data/inventory.csv`, groups inventory by category, and prints a JSON
report. The fixture includes exactly 15 tests and a small amount of intentional
dead code so benchmark tasks have stable facts to discover.

Do not edit `benchmarks/workspace/` directly. Rebuild it from this tracked
template with:

```sh
python3 benchmarks/rebuild_workspace.py --force
```
