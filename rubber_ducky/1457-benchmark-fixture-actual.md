# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1457-benchmark-fixture-v2" -->

<!-- event id="request" artifact path="1457-benchmark-fixture-actual/artifacts/round-1-review-request.diff" sha256="39d06c372c35c43c2c5c5e40222eb26b99c1c2e890a8d94c269325490b2c67dd" -->
## Review Request — Round 1
**Task:** 1457 — Reconstruct reproducible benchmark fixture and API-free validation after OS reinstall
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Track the previously gitignored Rust inventory fixture as a source template, recreate the mutable nested Git workspace with a deterministic Python builder and seven-commit history, lock its generated commit fingerprint in tests, and document the workflow. Preserve historical benchmark lineage by assigning the reconstructed fixture a new identity.

### Relevant Code / Diff
diff --git i/benchmarks/README.md w/benchmarks/README.md
index 7e0dbdf..12339ed 100644
--- i/benchmarks/README.md
+++ w/benchmarks/README.md
@@ -20,7 +20,9 @@ keep current.
 | `analyze.py` | Aggregates one or more result dirs, grouped by model, correctness-gated. |
 | `context_compare.py` | Separates call-count and mean-context effects and ranks context components. |
 | `tasks/*.json` | The task suite (prompt + machine-checkable `checks`). |
-| `workspace/` | A git repo the agent operates in; reset (`git checkout` + `clean`) before each task. |
+| `workspace-template/` | Tracked source for the inventory-app fixture. |
+| `rebuild_workspace.py` | Recreates the ignored fixture with its deterministic seven-commit history. |
+| `workspace/` | Generated git repo the agent operates in; reset (`git checkout` + `clean`) before each task. |
 | `check_task.py` | Correctness gate: runs each task's `checks` against the response + workspace. |
 | `extract_tokens.py` | Normalizes the `--debug-tokens` log delta into a per-task summary JSON. |
 | `summarize.py` | Prints the end-of-run per-task summary table (shared by the runners). |
@@ -47,6 +49,9 @@ To benchmark a different model, change `DAIMONOS_AGENT_MODEL` in `agent.env`
 ## Quick start
 
 ```sh
+# Recreate the local fixture after cloning or reinstalling:
+python3 rebuild_workspace.py
+
 # Single-task smoke test FIRST — confirm plumbing, output, and billing before
 # spending on the full suite (the OpenRouter account has no hard spend cap):
 ./bench-agent.sh 01
@@ -59,6 +64,13 @@ python3 analyze.py results/
 python3 analyze.py results/ sol
 ```
 
+`workspace/` remains gitignored because benchmark tasks intentionally mutate
+and reset its nested Git repository. Its source is tracked in
+`workspace-template/`; `rebuild_workspace.py --force` replaces a stale or
+modified fixture. The current template deterministically generates fixture
+commit `4b9b19a45b4e82e07a7cd6db74bbd50900fe6890`. This is a new fixture lineage
+identity, not a substitute for historical fixture commits recorded below.
+
 ## Controlled tool-output benchmark
 
 `bench-tool-output.sh` runs the same scripted agent turn twice with the final
diff --git 1/benchmarks/rebuild_workspace.py 2/benchmarks/rebuild_workspace.py
new file mode 100755
index 0000000..89351f6
--- /dev/null
+++ 2/benchmarks/rebuild_workspace.py
@@ -0,0 +1,114 @@
+#!/usr/bin/env python3
+"""Recreate the gitignored inventory-app benchmark fixture deterministically."""
+
+from __future__ import annotations
+
+import argparse
+import os
+import shutil
+import subprocess
+from pathlib import Path
+
+
+SCRIPT_DIR = Path(__file__).resolve().parent
+TEMPLATE = SCRIPT_DIR / "workspace-template"
+DEFAULT_DESTINATION = SCRIPT_DIR / "workspace"
+HISTORY = [
+    "Bootstrap benchmark history",
+    "Add project README",
+    "Initial project setup",
+    "Add inventory data",
+    "Add configuration",
+    "Add gitignore",
+]
+FINAL_SUBJECT = "Initial benchmark workspace"
+
+
+def git(workspace: Path, *args: str, commit_number: int | None = None) -> None:
+    env = os.environ.copy()
+    if commit_number is not None:
+        timestamp = f"2026-01-01T00:00:{commit_number:02d}Z"
+        env.update(
+            {
+                "GIT_AUTHOR_DATE": timestamp,
+                "GIT_COMMITTER_DATE": timestamp,
+            }
+        )
+    subprocess.run(
+        ["git", *args],
+        cwd=workspace,
+        env=env,
+        check=True,
+        stdout=subprocess.DEVNULL,
+    )
+
+
+def rebuild(destination: Path, force: bool) -> str:
+    destination = destination.resolve()
+    if destination.exists():
+        if not force:
+            raise SystemExit(
+                f"{destination} already exists; pass --force to replace it"
+            )
+        shutil.rmtree(destination)
+
+    shutil.copytree(TEMPLATE, destination)
+    git(destination, "init", "-q", "-b", "master")
+    git(destination, "config", "user.name", "Daimonos Benchmark")
+    git(destination, "config", "user.email", "benchmark@daimonos.local")
+    git(destination, "config", "commit.gpgSign", "false")
+    git(destination, "config", "core.autocrlf", "false")
+
+    # Tasks 06 and 09 intentionally measure a seven-commit history. The first
+    # six commits are stable markers; the final commit owns the complete
+    # fixture, so `git diff --stat HEAD~1` always describes its source/config.
+    for number, subject in enumerate(HISTORY, start=1):
+        git(
+            destination,
+            "commit",
+            "-q",
+            "--allow-empty",
+            "-m",
+            subject,
+            commit_number=number,
+        )
+
+    git(destination, "add", ".")
+    git(
+        destination,
+        "commit",
+        "-q",
+        "-m",
+        FINAL_SUBJECT,
+        commit_number=len(HISTORY) + 1,
+    )
+    return subprocess.run(
+        ["git", "rev-parse", "HEAD"],
+        cwd=destination,
+        check=True,
+        capture_output=True,
+        text=True,
+    ).stdout.strip()
+
+
+def main() -> None:
+    parser = argparse.ArgumentParser(description=__doc__)
+    parser.add_argument(
+        "--destination",
+        type=Path,
+        default=DEFAULT_DESTINATION,
+        help="workspace to create (default: benchmarks/workspace)",
+    )
+    parser.add_argument(
+        "--force",
+        action="store_true",
+        help="replace an existing destination",
+    )
+    args = parser.parse_args()
+    commit = rebuild(args.destination, args.force)
+    print(f"Rebuilt {args.destination.resolve()}")
+    print(f"Fixture commit: {commit}")
+
+
+if __name__ == "__main__":
+    main()
diff --git 1/benchmarks/workspace-template/.gitignore 2/benchmarks/workspace-template/.gitignore
new file mode 100644
index 0000000..d270ba9
--- /dev/null
+++ 2/benchmarks/workspace-template/.gitignore
@@ -0,0 +1,2 @@
+/target/
+/.daimonos/
diff --git 1/benchmarks/workspace-template/Cargo.toml 2/benchmarks/workspace-template/Cargo.toml
new file mode 100644
index 0000000..1fa2a66
--- /dev/null
+++ 2/benchmarks/workspace-template/Cargo.toml
@@ -0,0 +1,9 @@
+[package]
+name = "inventory-benchmark"
+version = "0.1.0"
+edition = "2021"
+
+[dependencies]
+csv = "1"
+serde = { version = "1", features = ["derive"] }
+serde_json = "1"
diff --git 1/benchmarks/workspace-template/Cargo.lock 2/benchmarks/workspace-template/Cargo.lock
new file mode 100644
index 0000000..260de5e
--- /dev/null
+++ 2/benchmarks/workspace-template/Cargo.lock
@@ -0,0 +1,135 @@
+# This file is automatically @generated by Cargo.
+# It is not intended for manual editing.
+version = 4
+
+[[package]]
+name = "csv"
+version = "1.4.0"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "52cd9d68cf7efc6ddfaaee42e7288d3a99d613d4b50f76ce9827ae0c6e14f938"
+dependencies = [
+ "csv-core",
+ "itoa",
+ "ryu",
+ "serde_core",
+]
+
+[[package]]
+name = "csv-core"
+version = "0.1.13"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "704a3c26996a80471189265814dbc2c257598b96b8a7feae2d31ace646bb9782"
+dependencies = [
+ "memchr",
+]
+
+[[package]]
+name = "inventory-benchmark"
+version = "0.1.0"
+dependencies = [
+ "csv",
+ "serde",
+ "serde_json",
+]
+
+[[package]]
+name = "itoa"
+version = "1.0.18"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "8f42a60cbdf9a97f5d2305f08a87dc4e09308d1276d28c869c684d7777685682"
+
+[[package]]
+name = "memchr"
+version = "2.8.3"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "cf8baf1c55e62ffcace7a9f06f4bd9cd3f0c4beb022d3b367256b91b87513d98"
+
+[[package]]
+name = "proc-macro2"
+version = "1.0.107"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "985e7ec9bb745e6ce6535b544d84d6cd6f7ad8bd711c398938ae983b91a766d9"
+dependencies = [
+ "unicode-ident",
+]
+
+[[package]]
+name = "quote"
+version = "1.0.47"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "1fbf4db142a473a8d80c26bbf18454ed458bf8d26c8219c331daecfdbd079001"
+dependencies = [
+ "proc-macro2",
+]
+
+[[package]]
+name = "ryu"
+version = "1.0.23"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "9774ba4a74de5f7b1c1451ed6cd5285a32eddb5cccb8cc655a4e50009e06477f"
+
+[[package]]
+name = "serde"
+version = "1.0.229"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "4148590afebada386688f18773da617792bf2ef03ffc1e4cbd2b1d45b023e0ba"
+dependencies = [
+ "serde_core",
+ "serde_derive",
+]
+
+[[package]]
+name = "serde_core"
+version = "1.0.229"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "67dca2c9c51e58a4791a4b1ed58308b39c64224d349a935ab5039aa360942a48"
+dependencies = [
+ "serde_derive",
+]
+
+[[package]]
+name = "serde_derive"
+version = "1.0.229"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "e7a5d71263a5a7d47b41f6b3f06ba276f10cc18b0931f1799f710578e2309348"
+dependencies = [
+ "proc-macro2",
+ "quote",
+ "syn",
+]
+
+[[package]]
+name = "serde_json"
+version = "1.0.151"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "c841b55ecdae098c80dcae9cf767f6f8a0c2cdb3416bbef72181df4d0fe73f14"
+dependencies = [
+ "itoa",
+ "memchr",
+ "serde",
+ "serde_core",
+ "zmij",
+]
+
+[[package]]
+name = "syn"
+version = "3.0.5"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "12df2e0110f65b775f769bb17ef989067a1d931b2eb822bd4346631eeada89f9"
+dependencies = [
+ "proc-macro2",
+ "quote",
+ "unicode-ident",
+]
+
+[[package]]
+name = "unicode-ident"
+version = "1.0.24"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "e6e4313cd5fcd3dad5cafa179702e2b244f760991f45397d14d4ebf38247da75"
+
+[[package]]
+name = "zmij"
+version = "1.0.23"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "29666d0abbfad1e3dc4dcf6144730dd3a3ab225bbbdac83319345b1b44ccfc1b"
diff --git 1/benchmarks/workspace-template/README.md 2/benchmarks/workspace-template/README.md
new file mode 100644
index 0000000..2bdc728
--- /dev/null
+++ 2/benchmarks/workspace-template/README.md
@@ -0,0 +1,13 @@
+# Inventory benchmark fixture
+
+A deliberately small Rust application used by the Daimonos agent benchmark.
+It loads `data/inventory.csv`, groups inventory by category, and prints a JSON
+report. The fixture includes exactly 15 tests and a small amount of intentional
+dead code so benchmark tasks have stable facts to discover.
+
+Do not edit `benchmarks/workspace/` directly. Rebuild it from this tracked
+template with:
+
+```sh
+python3 benchmarks/rebuild_workspace.py --force
+```
diff --git 1/benchmarks/workspace-template/data/inventory.csv 2/benchmarks/workspace-template/data/inventory.csv
new file mode 100644
index 0000000..82d743b
--- /dev/null
+++ 2/benchmarks/workspace-template/data/inventory.csv
@@ -0,0 +1,7 @@
+sku,name,category,quantity,unit_price
+ELEC-001,Wireless Mouse,electronics,12,24.99
+ELEC-002,Mechanical Keyboard,electronics,4,89.50
+HOME-001,Desk Lamp,home,7,34.00
+HOME-002,Storage Basket,home,3,18.75
+BOOK-001,Rust in Action,books,9,39.95
+BOOK-002,The Pragmatic Programmer,books,2,44.99
diff --git 1/benchmarks/workspace-template/src/config.rs 2/benchmarks/workspace-template/src/config.rs
new file mode 100644
index 0000000..b5041c2
--- /dev/null
+++ 2/benchmarks/workspace-template/src/config.rs
@@ -0,0 +1,50 @@
+use std::collections::HashMap;
+
+#[derive(Debug, Clone)]
+pub struct AppConfig {
+    pub low_stock_threshold: u32,
+    category_labels: HashMap<String, String>,
+}
+
+impl AppConfig {
+    pub fn category_display_name(&self, category: &str) -> String {
+        self.category_labels
+            .get(category)
+            .cloned()
+            .unwrap_or_else(|| category.to_string())
+    }
+}
+
+pub fn default_config() -> AppConfig {
+    AppConfig {
+        low_stock_threshold: 5,
+        category_labels: HashMap::from([
+            ("books".to_string(), "Books".to_string()),
+            ("electronics".to_string(), "Electronics".to_string()),
+            ("home".to_string(), "Home & Garden".to_string()),
+        ]),
+    }
+}
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+
+    #[test]
+    fn test_default_low_stock_threshold() {
+        assert_eq!(default_config().low_stock_threshold, 5);
+    }
+
+    #[test]
+    fn test_known_category_display_name() {
+        assert_eq!(
+            default_config().category_display_name("electronics"),
+            "Electronics"
+        );
+    }
+
+    #[test]
+    fn test_unknown_category_display_name() {
+        assert_eq!(default_config().category_display_name("other"), "other");
+    }
+}
diff --git 1/benchmarks/workspace-template/src/inventory.rs 2/benchmarks/workspace-template/src/inventory.rs
new file mode 100644
index 0000000..e58039b
--- /dev/null
+++ 2/benchmarks/workspace-template/src/inventory.rs
@@ -0,0 +1,147 @@
+use serde::{Deserialize, Serialize};
+use std::collections::HashMap;
+use std::io::Read;
+
+#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
+pub struct Item {
+    pub sku: String,
+    pub name: String,
+    pub category: String,
+    pub quantity: u32,
+    pub unit_price: f64,
+}
+
+impl Item {
+    pub fn is_low_stock(&self, threshold: u32) -> bool {
+        self.quantity <= threshold
+    }
+
+    pub fn total_value(&self) -> f64 {
+        self.quantity as f64 * self.unit_price
+    }
+
+    pub fn apply_discount(&mut self, percent: f64) {
+        self.unit_price *= 1.0 - percent / 100.0;
+    }
+}
+
+#[derive(Debug, Default)]
+pub struct Inventory {
+    items: Vec<Item>,
+}
+
+impl Inventory {
+    pub fn new(items: Vec<Item>) -> Self {
+        Self { items }
+    }
+
+    pub fn from_csv(reader: impl Read) -> Result<Self, csv::Error> {
+        let mut csv_reader = csv::Reader::from_reader(reader);
+        let items = csv_reader.deserialize().collect::<Result<Vec<_>, _>>()?;
+        Ok(Self::new(items))
+    }
+
+    pub fn items(&self) -> &[Item] {
+        &self.items
+    }
+
+    pub fn find_by_sku(&self, sku: &str) -> Option<&Item> {
+        self.items.iter().find(|item| item.sku == sku)
+    }
+
+    pub fn find_by_category(&self, category: &str) -> Vec<&Item> {
+        self.items
+            .iter()
+            .filter(|item| item.category == category)
+            .collect()
+    }
+
+    pub fn low_stock_items(&self, threshold: u32) -> Vec<&Item> {
+        self.items
+            .iter()
+            .filter(|item| item.is_low_stock(threshold))
+            .collect()
+    }
+
+    pub fn total_inventory_value(&self) -> f64 {
+        self.items.iter().map(Item::total_value).sum()
+    }
+
+    pub fn items_by_category(&self) -> HashMap<&str, Vec<&Item>> {
+        let mut grouped = HashMap::new();
+        for item in &self.items {
+            grouped
+                .entry(item.category.as_str())
+                .or_insert_with(Vec::new)
+                .push(item);
+        }
+        grouped
+    }
+}
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+
+    fn item(sku: &str, category: &str, quantity: u32, unit_price: f64) -> Item {
+        Item {
+            sku: sku.to_string(),
+            name: format!("Item {sku}"),
+            category: category.to_string(),
+            quantity,
+            unit_price,
+        }
+    }
+
+    #[test]
+    fn test_new_inventory() {
+        assert!(Inventory::new(Vec::new()).items().is_empty());
+    }
+
+    #[test]
+    fn test_from_csv() {
+        let input = "sku,name,category,quantity,unit_price\nA1,Mouse,electronics,2,25.0\n";
+        let inventory = Inventory::from_csv(input.as_bytes()).unwrap();
+        assert_eq!(inventory.items().len(), 1);
+        assert_eq!(inventory.items()[0].sku, "A1");
+    }
+
+    #[test]
+    fn test_find_by_sku() {
+        let inventory = Inventory::new(vec![item("A1", "books", 2, 10.0)]);
+        assert_eq!(inventory.find_by_sku("A1").unwrap().name, "Item A1");
+        assert!(inventory.find_by_sku("missing").is_none());
+    }
+
+    #[test]
+    fn test_find_by_category() {
+        let inventory = Inventory::new(vec![
+            item("A1", "books", 2, 10.0),
+            item("A2", "home", 3, 12.0),
+        ]);
+        assert_eq!(inventory.find_by_category("books").len(), 1);
+    }
+
+    #[test]
+    fn test_low_stock_items() {
+        let inventory = Inventory::new(vec![
+            item("A1", "books", 2, 10.0),
+            item("A2", "books", 8, 12.0),
+        ]);
+        assert_eq!(inventory.low_stock_items(5).len(), 1);
+    }
+
+    #[test]
+    fn test_total_value() {
+        assert_eq!(item("A1", "books", 3, 12.5).total_value(), 37.5);
+    }
+
+    #[test]
+    fn test_total_inventory_value() {
+        let inventory = Inventory::new(vec![
+            item("A1", "books", 2, 10.0),
+            item("A2", "home", 3, 12.0),
+        ]);
+        assert_eq!(inventory.total_inventory_value(), 56.0);
+    }
+}
diff --git 1/benchmarks/workspace-template/src/main.rs 2/benchmarks/workspace-template/src/main.rs
new file mode 100644
index 0000000..4ab4885
--- /dev/null
+++ 2/benchmarks/workspace-template/src/main.rs
@@ -0,0 +1,18 @@
+mod config;
+mod inventory;
+mod report;
+
+use config::default_config;
+use inventory::Inventory;
+use report::Report;
+use std::fs::File;
+
+fn main() -> Result<(), Box<dyn std::error::Error>> {
+    let config = default_config();
+    let inventory = Inventory::from_csv(File::open("data/inventory.csv")?)?;
+
+    let _category_count = inventory.items_by_category().len();
+    let report = Report::generate(&inventory, &config);
+    println!("{}", serde_json::to_string_pretty(&report)?);
+    Ok(())
+}
diff --git 1/benchmarks/workspace-template/src/report.rs 2/benchmarks/workspace-template/src/report.rs
new file mode 100644
index 0000000..d7966b7
--- /dev/null
+++ 2/benchmarks/workspace-template/src/report.rs
@@ -0,0 +1,121 @@
+use crate::config::AppConfig;
+use crate::inventory::{Inventory, Item};
+use serde::Serialize;
+
+#[derive(Debug, PartialEq, Serialize)]
+pub struct CategorySummary {
+    pub category: String,
+    pub display_name: String,
+    pub item_count: usize,
+    pub quantity: u32,
+    pub total_value: f64,
+}
+
+impl CategorySummary {
+    fn summarize_item(item: &Item, config: &AppConfig) -> Self {
+        Self {
+            category: item.category.clone(),
+            display_name: config.category_display_name(&item.category),
+            item_count: 1,
+            quantity: item.quantity,
+            total_value: item.total_value(),
+        }
+    }
+}
+
+#[derive(Debug, PartialEq, Serialize)]
+pub struct Report {
+    pub item_count: usize,
+    pub total_value: f64,
+    pub low_stock_count: usize,
+    pub categories: Vec<CategorySummary>,
+}
+
+impl Report {
+    pub fn generate(inventory: &Inventory, config: &AppConfig) -> Self {
+        let mut categories = Vec::<CategorySummary>::new();
+        for item in inventory.items() {
+            if let Some(summary) = categories
+                .iter_mut()
+                .find(|summary| summary.category == item.category)
+            {
+                summary.item_count += 1;
+                summary.quantity += item.quantity;
+                summary.total_value += item.total_value();
+            } else {
+                categories.push(CategorySummary::summarize_item(item, config));
+            }
+        }
+        categories.sort_by(|left, right| left.category.cmp(&right.category));
+
+        Self {
+            item_count: inventory.items().len(),
+            total_value: inventory.total_inventory_value(),
+            low_stock_count: inventory.low_stock_items(config.low_stock_threshold).len(),
+            categories,
+        }
+    }
+}
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+    use crate::config::default_config;
+
+    fn item(sku: &str, category: &str, quantity: u32, unit_price: f64) -> Item {
+        Item {
+            sku: sku.to_string(),
+            name: format!("Item {sku}"),
+            category: category.to_string(),
+            quantity,
+            unit_price,
+        }
+    }
+
+    #[test]
+    fn test_summarize_item() {
+        let item = item("A1", "books", 2, 10.0);
+        let summary = CategorySummary::summarize_item(&item, &default_config());
+        assert_eq!(summary.display_name, "Books");
+        assert_eq!(summary.total_value, 20.0);
+    }
+
+    #[test]
+    fn test_empty_report() {
+        let report = Report::generate(&Inventory::new(Vec::new()), &default_config());
+        assert_eq!(report.item_count, 0);
+        assert!(report.categories.is_empty());
+    }
+
+    #[test]
+    fn test_generate_groups_categories() {
+        let inventory = Inventory::new(vec![
+            item("A1", "books", 2, 10.0),
+            item("A2", "books", 3, 12.0),
+            item("A3", "home", 1, 5.0),
+        ]);
+        let report = Report::generate(&inventory, &default_config());
+        assert_eq!(report.categories.len(), 2);
+        assert_eq!(report.categories[0].item_count, 2);
+    }
+
+    #[test]
+    fn test_generate_totals_inventory() {
+        let inventory = Inventory::new(vec![
+            item("A1", "books", 2, 10.0),
+            item("A2", "home", 3, 12.0),
+        ]);
+        assert_eq!(
+            Report::generate(&inventory, &default_config()).total_value,
+            56.0
+        );
+    }
+
+    #[test]
+    fn test_report_serializes_to_json() {
+        let inventory = Inventory::new(vec![item("A1", "books", 2, 10.0)]);
+        let json = serde_json::to_string(&Report::generate(&inventory, &default_config())).unwrap();
+        assert!(json.contains("\"categories\""));
+        assert!(json.contains("\"Books\""));
+    }
+}
diff --git 1/tests/test_benchmark_fixture.py 2/tests/test_benchmark_fixture.py
new file mode 100644
index 0000000..f6c0e66
--- /dev/null
+++ 2/tests/test_benchmark_fixture.py
@@ -0,0 +1,86 @@
+"""Regression tests for the reproducible agent-benchmark workspace."""
+
+from __future__ import annotations
+
+import subprocess
+import sys
+from pathlib import Path
+
+
+REPO_ROOT = Path(__file__).resolve().parents[1]
+BUILDER = REPO_ROOT / "benchmarks" / "rebuild_workspace.py"
+EXPECTED_FIXTURE_COMMIT = "4b9b19a45b4e82e07a7cd6db74bbd50900fe6890"
+
+
+def run(*args: str, cwd: Path) -> subprocess.CompletedProcess[str]:
+    return subprocess.run(
+        [*args],
+        cwd=cwd,
+        check=True,
+        capture_output=True,
+        text=True,
+        timeout=120,
+    )
+
+
+def test_rebuild_workspace_restores_benchmark_contract(tmp_path):
+    workspace = tmp_path / "workspace"
+    run(
+        sys.executable,
+        str(BUILDER),
+        "--destination",
+        str(workspace),
+        cwd=REPO_ROOT,
+    )
+
+    assert {
+        path.relative_to(workspace).as_posix()
+        for path in (workspace / "src").glob("*.rs")
+    } == {
+        "src/config.rs",
+        "src/inventory.rs",
+        "src/main.rs",
+        "src/report.rs",
+    }
+    assert (workspace / "data" / "inventory.csv").is_file()
+    assert (workspace / "src" / "config.rs").read_text().count("category_labels") == 3
+
+    assert run("git", "rev-list", "--count", "HEAD", cwd=workspace).stdout.strip() == "7"
+    assert (
+        run("git", "rev-parse", "HEAD", cwd=workspace).stdout.strip()
+        == EXPECTED_FIXTURE_COMMIT
+    )
+    assert (
+        run("git", "log", "-1", "--format=%s", cwd=workspace).stdout.strip()
+        == "Initial benchmark workspace"
+    )
+    assert run("git", "status", "--porcelain", cwd=workspace).stdout == ""
+
+    tests = run("cargo", "test", "--quiet", cwd=workspace)
+    assert "15 passed" in tests.stdout + tests.stderr
+    assert run("git", "status", "--porcelain", cwd=workspace).stdout == ""
+
+    clippy = run("cargo", "clippy", "--quiet", cwd=workspace)
+    diagnostics = clippy.stdout + clippy.stderr
+    assert "apply_discount" in diagnostics
+    assert "find_by_sku" in diagnostics
+    assert "find_by_category" in diagnostics
+
+
+def test_rebuild_workspace_force_replaces_mutated_fixture(tmp_path):
+    workspace = tmp_path / "workspace"
+    base = [
+        sys.executable,
+        str(BUILDER),
+        "--destination",
+        str(workspace),
+    ]
+    run(*base, cwd=REPO_ROOT)
+    initial_commit = run("git", "rev-parse", "HEAD", cwd=workspace).stdout
+    (workspace / "src" / "config.rs").write_text("broken\n")
+
+    run(*base, "--force", cwd=REPO_ROOT)
+
+    assert "category_labels" in (workspace / "src" / "config.rs").read_text()
+    assert run("git", "rev-parse", "HEAD", cwd=workspace).stdout == initial_commit
+    assert run("git", "status", "--porcelain", cwd=workspace).stdout == ""

### Known Concerns
1. The six pre-fixture history commits are deterministic empty markers rather than recovered historical content.
2. The --force path deliberately removes the entire caller-supplied destination.
3. The test pins a Git SHA that depends on exact template contents and commit construction.

### Specific Questions for Reviewer
1. Does the reconstruction faithfully satisfy existing benchmark task contracts without misleading historical comparability?
2. Are destructive rebuild semantics and deterministic Git construction safe and portable?
3. Do tests cover important lifecycle and failure cases?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. The pinned fixture SHA (4b9b19a45b4e82e07a7cd6db74bbd50900fe6890) is fragile beyond template contents: git commit hashes also depend on tree construction details that vary with git version and platform behavior (e.g., core.autocrlf overrides help but file mode differences on filesystems without executable-bit support, symlink handling, and future git object-format changes). At minimum, the builder should also pin GIT_AUTHOR_NAME/EMAIL and GIT_COMMITTER_NAME/EMAIL via environment (git config user.* does not protect against user.useConfigOnly or committer-identity overrides from GIT_COMMITTER_* env vars already present in the caller's environment), and set TZ or use explicit +0000 offsets. Currently, if a developer or CI has GIT_AUTHOR_NAME/GIT_COMMITTER_NAME/EMAIL exported, rebuild produces a different SHA and both the README claim and the test silently break. Sanitize the env passed to git (strip inherited GIT_* variables) and pass identity explicitly.
B2. rebuild(--force) calls shutil.rmtree on the resolved caller-supplied --destination with no safety validation. A typo such as --destination ~ or --destination / with --force will recursively delete arbitrary trees. Add a guardrail: refuse to remove a destination that does not look like a previously generated fixture (e.g., require it to contain a .git directory plus a sentinel file the builder writes, or restrict deletion to paths whose basename is 'workspace' / that live under the benchmarks dir unless an explicit --unsafe-allow-any-path flag is given).
B3. The regression test hard-fails without cargo/clippy installed and asserts on clippy diagnostic text ('apply_discount', 'find_by_sku', 'find_by_category' dead-code warnings), which is not stable across Rust toolchain versions — lint names, wording, and dead-code detection change between releases, and 'cargo clippy --quiet' emits nothing on a warm cache (warnings are only printed on fresh compilation; a cached target dir yields empty diagnostics and the assertions fail spuriously). Either force recompilation (e.g., 'cargo clippy --quiet -- -W dead_code' with a clean target or CARGO_TARGET_DIR in tmp) and gate the whole cargo section behind a skip-if-toolchain-missing marker, or split toolchain-dependent assertions into a separately marked test.

### Non-Blocking Suggestions
S1. The final commit's SHA depends on the index contents of 'git add .', which will include any OS junk files present in the template copy at build time (.DS_Store, editor swap files) since the template's .gitignore only excludes /target/ and /.daimonos/. Consider adding common junk patterns to the fixture .gitignore or using 'git add -A -- :/' after an explicit clean, to keep the deterministic SHA robust on macOS/Windows checkouts.
S2. The six empty marker commits satisfy the seven-commit count for tasks 06/09, but if any task inspects per-commit diffs or file introduction history (e.g., 'git log -- <file>' or blame-based checks), empty commits will not reproduce the original history semantics. Recommend auditing tasks/*.json checks against the reconstructed history and noting the audit result in the README, so the 'marker commits' design decision is verifiably safe rather than assumed.
S3. Deduplicate the expected fixture commit: it is currently pinned in both benchmarks/README.md and tests/test_benchmark_fixture.py. Store it once (e.g., a benchmarks/fixture-commit file or a constant emitted by rebuild_workspace.py --print-expected) and have the test and docs reference that single source, so a template change requires exactly one update.
S4. rebuild_workspace.py could verify the produced HEAD against the expected pinned SHA and warn (or fail with --strict) on mismatch, catching environment-induced nondeterminism at rebuild time instead of at test time.

**Verdict:** REVISE

<!-- event id="rebuttal-1" artifact path="1457-benchmark-fixture-actual/artifacts/round-1-rebuttal.diff" sha256="d8da2bee22639b0c3555a8c7c3e2f1578c31a76cd69c3fcc2443646b2dd1b24e" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — Valid: inherited GIT_* identity/config variables could change commit objects. The builder now strips inherited GIT_* variables, pins author/committer identity, UTC timestamps, locale, SHA-1 object format, global/system config isolation, and regular-file modes.
Re B2: ACCEPT — Valid: unrestricted shutil.rmtree was unsafe. Forced replacement now rejects symlinks and only permits the canonical generated workspace or a custom directory containing both the exact versioned sentinel and a nested .git directory; a regression test proves arbitrary data is preserved.
Re B3: ACCEPT — Valid: tool availability and warm-cache diagnostics were not controlled. Cargo and Clippy contracts are split into skip-gated tests, the fixture pins Rust 1.96.0, commands use --locked, and Clippy runs in a fresh CARGO_TARGET_DIR with -W dead_code.

### Revised Code / Diff
diff --git i/benchmarks/README.md w/benchmarks/README.md
index 7e0dbdf..61606b2 100644
--- i/benchmarks/README.md
+++ w/benchmarks/README.md
@@ -20,7 +20,9 @@ keep current.
 | `analyze.py` | Aggregates one or more result dirs, grouped by model, correctness-gated. |
 | `context_compare.py` | Separates call-count and mean-context effects and ranks context components. |
 | `tasks/*.json` | The task suite (prompt + machine-checkable `checks`). |
-| `workspace/` | A git repo the agent operates in; reset (`git checkout` + `clean`) before each task. |
+| `workspace-template/` | Tracked source for the inventory-app fixture. |
+| `rebuild_workspace.py` | Recreates the ignored fixture with its deterministic seven-commit history. |
+| `workspace/` | Generated git repo the agent operates in; reset (`git checkout` + `clean`) before each task. |
 | `check_task.py` | Correctness gate: runs each task's `checks` against the response + workspace. |
 | `extract_tokens.py` | Normalizes the `--debug-tokens` log delta into a per-task summary JSON. |
 | `summarize.py` | Prints the end-of-run per-task summary table (shared by the runners). |
@@ -47,6 +49,9 @@ To benchmark a different model, change `DAIMONOS_AGENT_MODEL` in `agent.env`
 ## Quick start
 
 ```sh
+# Recreate the local fixture after cloning or reinstalling:
+python3 rebuild_workspace.py
+
 # Single-task smoke test FIRST — confirm plumbing, output, and billing before
 # spending on the full suite (the OpenRouter account has no hard spend cap):
 ./bench-agent.sh 01
@@ -59,6 +64,16 @@ python3 analyze.py results/
 python3 analyze.py results/ sol
 ```
 
+`workspace/` remains gitignored because benchmark tasks intentionally mutate
+and reset its nested Git repository. Its source is tracked in
+`workspace-template/`; `rebuild_workspace.py --force` replaces a stale or
+modified fixture. `workspace-template.commit` is the single source of truth for
+the generated fixture commit. This is a new fixture lineage identity, not a
+substitute for historical fixture commits recorded below. The six deterministic
+marker commits preserve the facts exercised by tasks 06, 09, and 11: total
+commit count, recent subjects, and the final commit's changed-file list. No task
+depends on blame or per-file introduction history.
+
 ## Controlled tool-output benchmark
 
 `bench-tool-output.sh` runs the same scripted agent turn twice with the final
diff --git 1/benchmarks/rebuild_workspace.py 2/benchmarks/rebuild_workspace.py
new file mode 100755
index 0000000..5717663
--- /dev/null
+++ 2/benchmarks/rebuild_workspace.py
@@ -0,0 +1,172 @@
+#!/usr/bin/env python3
+"""Recreate the gitignored inventory-app benchmark fixture deterministically."""
+
+from __future__ import annotations
+
+import argparse
+import os
+import shutil
+import subprocess
+from pathlib import Path
+
+
+SCRIPT_DIR = Path(__file__).resolve().parent
+TEMPLATE = SCRIPT_DIR / "workspace-template"
+DEFAULT_DESTINATION = SCRIPT_DIR / "workspace"
+FINGERPRINT_FILE = SCRIPT_DIR / "workspace-template.commit"
+SENTINEL = ".daimonos-benchmark-fixture"
+SENTINEL_CONTENT = "daimonos-benchmark-fixture-v1"
+HISTORY = [
+    "Bootstrap benchmark history",
+    "Add project README",
+    "Initial project setup",
+    "Add inventory data",
+    "Add configuration",
+    "Add gitignore",
+]
+FINAL_SUBJECT = "Initial benchmark workspace"
+
+
+def git_environment(commit_number: int | None) -> dict[str, str]:
+    env = {
+        key: value
+        for key, value in os.environ.items()
+        if not key.startswith("GIT_")
+    }
+    env.update(
+        {
+            "GIT_CONFIG_GLOBAL": os.devnull,
+            "GIT_CONFIG_NOSYSTEM": "1",
+            "GIT_AUTHOR_NAME": "Daimonos Benchmark",
+            "GIT_AUTHOR_EMAIL": "benchmark@daimonos.local",
+            "GIT_COMMITTER_NAME": "Daimonos Benchmark",
+            "GIT_COMMITTER_EMAIL": "benchmark@daimonos.local",
+            "LC_ALL": "C",
+            "TZ": "UTC",
+        }
+    )
+    if commit_number is not None:
+        timestamp = f"2026-01-01T00:00:{commit_number:02d}+00:00"
+        env["GIT_AUTHOR_DATE"] = timestamp
+        env["GIT_COMMITTER_DATE"] = timestamp
+    return env
+
+
+def git(workspace: Path, *args: str, commit_number: int | None = None) -> None:
+    subprocess.run(
+        ["git", *args],
+        cwd=workspace,
+        env=git_environment(commit_number),
+        check=True,
+        stdout=subprocess.DEVNULL,
+    )
+
+
+def safe_destination(raw_destination: Path) -> Path:
+    destination = Path(os.path.abspath(raw_destination.expanduser()))
+    if destination.is_symlink():
+        raise SystemExit(f"refusing to replace symlink destination: {destination}")
+    return destination
+
+
+def validate_existing_fixture(destination: Path) -> None:
+    if destination == Path(os.path.abspath(DEFAULT_DESTINATION)):
+        return
+    sentinel = destination / SENTINEL
+    is_fixture = (
+        (destination / ".git").is_dir()
+        and sentinel.is_file()
+        and sentinel.read_text().strip() == SENTINEL_CONTENT
+    )
+    if not is_fixture:
+        raise SystemExit(
+            f"refusing to replace non-fixture directory: {destination}"
+        )
+
+
+def normalize_template_modes(destination: Path) -> None:
+    for path in destination.rglob("*"):
+        if path.is_symlink():
+            raise SystemExit(f"fixture template must not contain symlinks: {path}")
+        if path.is_file():
+            path.chmod(0o644)
+
+
+def rebuild(destination: Path, force: bool) -> str:
+    destination = safe_destination(destination)
+    if destination.exists():
+        if not force:
+            raise SystemExit(
+                f"{destination} already exists; pass --force to replace it"
+            )
+        validate_existing_fixture(destination)
+        shutil.rmtree(destination)
+
+    shutil.copytree(TEMPLATE, destination)
+    normalize_template_modes(destination)
+    git(destination, "init", "-q", "--object-format=sha1", "-b", "master")
+    git(destination, "config", "user.name", "Daimonos Benchmark")
+    git(destination, "config", "user.email", "benchmark@daimonos.local")
+    git(destination, "config", "commit.gpgSign", "false")
+    git(destination, "config", "core.autocrlf", "false")
+
+    # Tasks 06 and 09 intentionally measure a seven-commit history. The first
+    # six commits are stable markers; the final commit owns the complete
+    # fixture, so `git diff --stat HEAD~1` always describes its source/config.
+    for number, subject in enumerate(HISTORY, start=1):
+        git(
+            destination,
+            "commit",
+            "-q",
+            "--allow-empty",
+            "-m",
+            subject,
+            commit_number=number,
+        )
+
+    git(destination, "add", ".")
+    git(
+        destination,
+        "commit",
+        "-q",
+        "-m",
+        FINAL_SUBJECT,
+        commit_number=len(HISTORY) + 1,
+    )
+    commit = subprocess.run(
+        ["git", "rev-parse", "HEAD"],
+        cwd=destination,
+        check=True,
+        capture_output=True,
+        text=True,
+        env=git_environment(None),
+    ).stdout.strip()
+    expected = FINGERPRINT_FILE.read_text().strip()
+    if commit != expected:
+        raise SystemExit(
+            f"fixture fingerprint mismatch: generated {commit}, expected {expected}"
+        )
+    return commit
+
+
+def main() -> None:
+    parser = argparse.ArgumentParser(description=__doc__)
+    parser.add_argument(
+        "--destination",
+        type=Path,
+        default=DEFAULT_DESTINATION,
+        help="workspace to create (default: benchmarks/workspace)",
+    )
+    parser.add_argument(
+        "--force",
+        action="store_true",
+        help="replace an existing destination",
+    )
+    args = parser.parse_args()
+    commit = rebuild(args.destination, args.force)
+    print(f"Rebuilt {args.destination.resolve()}")
+    print(f"Fixture commit: {commit}")
+
+
+if __name__ == "__main__":
+    main()
diff --git 1/benchmarks/workspace-template.commit 2/benchmarks/workspace-template.commit
new file mode 100644
index 0000000..f82659a
--- /dev/null
+++ 2/benchmarks/workspace-template.commit
@@ -0,0 +1 @@
+b13a1f3aa25a7162895825cd8ee11ab4f1a6dc16
diff --git 1/benchmarks/workspace-template/.daimonos-benchmark-fixture 2/benchmarks/workspace-template/.daimonos-benchmark-fixture
new file mode 100644
index 0000000..414c028
--- /dev/null
+++ 2/benchmarks/workspace-template/.daimonos-benchmark-fixture
@@ -0,0 +1 @@
+daimonos-benchmark-fixture-v1
diff --git 1/benchmarks/workspace-template/.gitignore 2/benchmarks/workspace-template/.gitignore
new file mode 100644
index 0000000..f41a7a8
--- /dev/null
+++ 2/benchmarks/workspace-template/.gitignore
@@ -0,0 +1,5 @@
+/target/
+/.daimonos/
+.DS_Store
+*.swp
+*~
diff --git 1/benchmarks/workspace-template/Cargo.toml 2/benchmarks/workspace-template/Cargo.toml
new file mode 100644
index 0000000..1fa2a66
--- /dev/null
+++ 2/benchmarks/workspace-template/Cargo.toml
@@ -0,0 +1,9 @@
+[package]
+name = "inventory-benchmark"
+version = "0.1.0"
+edition = "2021"
+
+[dependencies]
+csv = "1"
+serde = { version = "1", features = ["derive"] }
+serde_json = "1"
diff --git 1/benchmarks/workspace-template/Cargo.lock 2/benchmarks/workspace-template/Cargo.lock
new file mode 100644
index 0000000..260de5e
--- /dev/null
+++ 2/benchmarks/workspace-template/Cargo.lock
@@ -0,0 +1,135 @@
+# This file is automatically @generated by Cargo.
+# It is not intended for manual editing.
+version = 4
+
+[[package]]
+name = "csv"
+version = "1.4.0"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "52cd9d68cf7efc6ddfaaee42e7288d3a99d613d4b50f76ce9827ae0c6e14f938"
+dependencies = [
+ "csv-core",
+ "itoa",
+ "ryu",
+ "serde_core",
+]
+
+[[package]]
+name = "csv-core"
+version = "0.1.13"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "704a3c26996a80471189265814dbc2c257598b96b8a7feae2d31ace646bb9782"
+dependencies = [
+ "memchr",
+]
+
+[[package]]
+name = "inventory-benchmark"
+version = "0.1.0"
+dependencies = [
+ "csv",
+ "serde",
+ "serde_json",
+]
+
+[[package]]
+name = "itoa"
+version = "1.0.18"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "8f42a60cbdf9a97f5d2305f08a87dc4e09308d1276d28c869c684d7777685682"
+
+[[package]]
+name = "memchr"
+version = "2.8.3"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "cf8baf1c55e62ffcace7a9f06f4bd9cd3f0c4beb022d3b367256b91b87513d98"
+
+[[package]]
+name = "proc-macro2"
+version = "1.0.107"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "985e7ec9bb745e6ce6535b544d84d6cd6f7ad8bd711c398938ae983b91a766d9"
+dependencies = [
+ "unicode-ident",
+]
+
+[[package]]
+name = "quote"
+version = "1.0.47"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "1fbf4db142a473a8d80c26bbf18454ed458bf8d26c8219c331daecfdbd079001"
+dependencies = [
+ "proc-macro2",
+]
+
+[[package]]
+name = "ryu"
+version = "1.0.23"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "9774ba4a74de5f7b1c1451ed6cd5285a32eddb5cccb8cc655a4e50009e06477f"
+
+[[package]]
+name = "serde"
+version = "1.0.229"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "4148590afebada386688f18773da617792bf2ef03ffc1e4cbd2b1d45b023e0ba"
+dependencies = [
+ "serde_core",
+ "serde_derive",
+]
+
+[[package]]
+name = "serde_core"
+version = "1.0.229"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "67dca2c9c51e58a4791a4b1ed58308b39c64224d349a935ab5039aa360942a48"
+dependencies = [
+ "serde_derive",
+]
+
+[[package]]
+name = "serde_derive"
+version = "1.0.229"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "e7a5d71263a5a7d47b41f6b3f06ba276f10cc18b0931f1799f710578e2309348"
+dependencies = [
+ "proc-macro2",
+ "quote",
+ "syn",
+]
+
+[[package]]
+name = "serde_json"
+version = "1.0.151"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "c841b55ecdae098c80dcae9cf767f6f8a0c2cdb3416bbef72181df4d0fe73f14"
+dependencies = [
+ "itoa",
+ "memchr",
+ "serde",
+ "serde_core",
+ "zmij",
+]
+
+[[package]]
+name = "syn"
+version = "3.0.5"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "12df2e0110f65b775f769bb17ef989067a1d931b2eb822bd4346631eeada89f9"
+dependencies = [
+ "proc-macro2",
+ "quote",
+ "unicode-ident",
+]
+
+[[package]]
+name = "unicode-ident"
+version = "1.0.24"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "e6e4313cd5fcd3dad5cafa179702e2b244f760991f45397d14d4ebf38247da75"
+
+[[package]]
+name = "zmij"
+version = "1.0.23"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "29666d0abbfad1e3dc4dcf6144730dd3a3ab225bbbdac83319345b1b44ccfc1b"
diff --git 1/benchmarks/workspace-template/README.md 2/benchmarks/workspace-template/README.md
new file mode 100644
index 0000000..2bdc728
--- /dev/null
+++ 2/benchmarks/workspace-template/README.md
@@ -0,0 +1,13 @@
+# Inventory benchmark fixture
+
+A deliberately small Rust application used by the Daimonos agent benchmark.
+It loads `data/inventory.csv`, groups inventory by category, and prints a JSON
+report. The fixture includes exactly 15 tests and a small amount of intentional
+dead code so benchmark tasks have stable facts to discover.
+
+Do not edit `benchmarks/workspace/` directly. Rebuild it from this tracked
+template with:
+
+```sh
+python3 benchmarks/rebuild_workspace.py --force
+```
diff --git 1/benchmarks/workspace-template/rust-toolchain.toml 2/benchmarks/workspace-template/rust-toolchain.toml
new file mode 100644
index 0000000..80e6a12
--- /dev/null
+++ 2/benchmarks/workspace-template/rust-toolchain.toml
@@ -0,0 +1,4 @@
+[toolchain]
+channel = "1.96.0"
+components = ["clippy", "rustfmt"]
+profile = "minimal"
diff --git 1/benchmarks/workspace-template/data/inventory.csv 2/benchmarks/workspace-template/data/inventory.csv
new file mode 100644
index 0000000..82d743b
--- /dev/null
+++ 2/benchmarks/workspace-template/data/inventory.csv
@@ -0,0 +1,7 @@
+sku,name,category,quantity,unit_price
+ELEC-001,Wireless Mouse,electronics,12,24.99
+ELEC-002,Mechanical Keyboard,electronics,4,89.50
+HOME-001,Desk Lamp,home,7,34.00
+HOME-002,Storage Basket,home,3,18.75
+BOOK-001,Rust in Action,books,9,39.95
+BOOK-002,The Pragmatic Programmer,books,2,44.99
diff --git 1/benchmarks/workspace-template/src/config.rs 2/benchmarks/workspace-template/src/config.rs
new file mode 100644
index 0000000..b5041c2
--- /dev/null
+++ 2/benchmarks/workspace-template/src/config.rs
@@ -0,0 +1,50 @@
+use std::collections::HashMap;
+
+#[derive(Debug, Clone)]
+pub struct AppConfig {
+    pub low_stock_threshold: u32,
+    category_labels: HashMap<String, String>,
+}
+
+impl AppConfig {
+    pub fn category_display_name(&self, category: &str) -> String {
+        self.category_labels
+            .get(category)
+            .cloned()
+            .unwrap_or_else(|| category.to_string())
+    }
+}
+
+pub fn default_config() -> AppConfig {
+    AppConfig {
+        low_stock_threshold: 5,
+        category_labels: HashMap::from([
+            ("books".to_string(), "Books".to_string()),
+            ("electronics".to_string(), "Electronics".to_string()),
+            ("home".to_string(), "Home & Garden".to_string()),
+        ]),
+    }
+}
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+
+    #[test]
+    fn test_default_low_stock_threshold() {
+        assert_eq!(default_config().low_stock_threshold, 5);
+    }
+
+    #[test]
+    fn test_known_category_display_name() {
+        assert_eq!(
+            default_config().category_display_name("electronics"),
+            "Electronics"
+        );
+    }
+
+    #[test]
+    fn test_unknown_category_display_name() {
+        assert_eq!(default_config().category_display_name("other"), "other");
+    }
+}
diff --git 1/benchmarks/workspace-template/src/inventory.rs 2/benchmarks/workspace-template/src/inventory.rs
new file mode 100644
index 0000000..e58039b
--- /dev/null
+++ 2/benchmarks/workspace-template/src/inventory.rs
@@ -0,0 +1,147 @@
+use serde::{Deserialize, Serialize};
+use std::collections::HashMap;
+use std::io::Read;
+
+#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
+pub struct Item {
+    pub sku: String,
+    pub name: String,
+    pub category: String,
+    pub quantity: u32,
+    pub unit_price: f64,
+}
+
+impl Item {
+    pub fn is_low_stock(&self, threshold: u32) -> bool {
+        self.quantity <= threshold
+    }
+
+    pub fn total_value(&self) -> f64 {
+        self.quantity as f64 * self.unit_price
+    }
+
+    pub fn apply_discount(&mut self, percent: f64) {
+        self.unit_price *= 1.0 - percent / 100.0;
+    }
+}
+
+#[derive(Debug, Default)]
+pub struct Inventory {
+    items: Vec<Item>,
+}
+
+impl Inventory {
+    pub fn new(items: Vec<Item>) -> Self {
+        Self { items }
+    }
+
+    pub fn from_csv(reader: impl Read) -> Result<Self, csv::Error> {
+        let mut csv_reader = csv::Reader::from_reader(reader);
+        let items = csv_reader.deserialize().collect::<Result<Vec<_>, _>>()?;
+        Ok(Self::new(items))
+    }
+
+    pub fn items(&self) -> &[Item] {
+        &self.items
+    }
+
+    pub fn find_by_sku(&self, sku: &str) -> Option<&Item> {
+        self.items.iter().find(|item| item.sku == sku)
+    }
+
+    pub fn find_by_category(&self, category: &str) -> Vec<&Item> {
+        self.items
+            .iter()
+            .filter(|item| item.category == category)
+            .collect()
+    }
+
+    pub fn low_stock_items(&self, threshold: u32) -> Vec<&Item> {
+        self.items
+            .iter()
+            .filter(|item| item.is_low_stock(threshold))
+            .collect()
+    }
+
+    pub fn total_inventory_value(&self) -> f64 {
+        self.items.iter().map(Item::total_value).sum()
+    }
+
+    pub fn items_by_category(&self) -> HashMap<&str, Vec<&Item>> {
+        let mut grouped = HashMap::new();
+        for item in &self.items {
+            grouped
+                .entry(item.category.as_str())
+                .or_insert_with(Vec::new)
+                .push(item);
+        }
+        grouped
+    }
+}
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+
+    fn item(sku: &str, category: &str, quantity: u32, unit_price: f64) -> Item {
+        Item {
+            sku: sku.to_string(),
+            name: format!("Item {sku}"),
+            category: category.to_string(),
+            quantity,
+            unit_price,
+        }
+    }
+
+    #[test]
+    fn test_new_inventory() {
+        assert!(Inventory::new(Vec::new()).items().is_empty());
+    }
+
+    #[test]
+    fn test_from_csv() {
+        let input = "sku,name,category,quantity,unit_price\nA1,Mouse,electronics,2,25.0\n";
+        let inventory = Inventory::from_csv(input.as_bytes()).unwrap();
+        assert_eq!(inventory.items().len(), 1);
+        assert_eq!(inventory.items()[0].sku, "A1");
+    }
+
+    #[test]
+    fn test_find_by_sku() {
+        let inventory = Inventory::new(vec![item("A1", "books", 2, 10.0)]);
+        assert_eq!(inventory.find_by_sku("A1").unwrap().name, "Item A1");
+        assert!(inventory.find_by_sku("missing").is_none());
+    }
+
+    #[test]
+    fn test_find_by_category() {
+        let inventory = Inventory::new(vec![
+            item("A1", "books", 2, 10.0),
+            item("A2", "home", 3, 12.0),
+        ]);
+        assert_eq!(inventory.find_by_category("books").len(), 1);
+    }
+
+    #[test]
+    fn test_low_stock_items() {
+        let inventory = Inventory::new(vec![
+            item("A1", "books", 2, 10.0),
+            item("A2", "books", 8, 12.0),
+        ]);
+        assert_eq!(inventory.low_stock_items(5).len(), 1);
+    }
+
+    #[test]
+    fn test_total_value() {
+        assert_eq!(item("A1", "books", 3, 12.5).total_value(), 37.5);
+    }
+
+    #[test]
+    fn test_total_inventory_value() {
+        let inventory = Inventory::new(vec![
+            item("A1", "books", 2, 10.0),
+            item("A2", "home", 3, 12.0),
+        ]);
+        assert_eq!(inventory.total_inventory_value(), 56.0);
+    }
+}
diff --git 1/benchmarks/workspace-template/src/main.rs 2/benchmarks/workspace-template/src/main.rs
new file mode 100644
index 0000000..4ab4885
--- /dev/null
+++ 2/benchmarks/workspace-template/src/main.rs
@@ -0,0 +1,18 @@
+mod config;
+mod inventory;
+mod report;
+
+use config::default_config;
+use inventory::Inventory;
+use report::Report;
+use std::fs::File;
+
+fn main() -> Result<(), Box<dyn std::error::Error>> {
+    let config = default_config();
+    let inventory = Inventory::from_csv(File::open("data/inventory.csv")?)?;
+
+    let _category_count = inventory.items_by_category().len();
+    let report = Report::generate(&inventory, &config);
+    println!("{}", serde_json::to_string_pretty(&report)?);
+    Ok(())
+}
diff --git 1/benchmarks/workspace-template/src/report.rs 2/benchmarks/workspace-template/src/report.rs
new file mode 100644
index 0000000..d7966b7
--- /dev/null
+++ 2/benchmarks/workspace-template/src/report.rs
@@ -0,0 +1,121 @@
+use crate::config::AppConfig;
+use crate::inventory::{Inventory, Item};
+use serde::Serialize;
+
+#[derive(Debug, PartialEq, Serialize)]
+pub struct CategorySummary {
+    pub category: String,
+    pub display_name: String,
+    pub item_count: usize,
+    pub quantity: u32,
+    pub total_value: f64,
+}
+
+impl CategorySummary {
+    fn summarize_item(item: &Item, config: &AppConfig) -> Self {
+        Self {
+            category: item.category.clone(),
+            display_name: config.category_display_name(&item.category),
+            item_count: 1,
+            quantity: item.quantity,
+            total_value: item.total_value(),
+        }
+    }
+}
+
+#[derive(Debug, PartialEq, Serialize)]
+pub struct Report {
+    pub item_count: usize,
+    pub total_value: f64,
+    pub low_stock_count: usize,
+    pub categories: Vec<CategorySummary>,
+}
+
+impl Report {
+    pub fn generate(inventory: &Inventory, config: &AppConfig) -> Self {
+        let mut categories = Vec::<CategorySummary>::new();
+        for item in inventory.items() {
+            if let Some(summary) = categories
+                .iter_mut()
+                .find(|summary| summary.category == item.category)
+            {
+                summary.item_count += 1;
+                summary.quantity += item.quantity;
+                summary.total_value += item.total_value();
+            } else {
+                categories.push(CategorySummary::summarize_item(item, config));
+            }
+        }
+        categories.sort_by(|left, right| left.category.cmp(&right.category));
+
+        Self {
+            item_count: inventory.items().len(),
+            total_value: inventory.total_inventory_value(),
+            low_stock_count: inventory.low_stock_items(config.low_stock_threshold).len(),
+            categories,
+        }
+    }
+}
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+    use crate::config::default_config;
+
+    fn item(sku: &str, category: &str, quantity: u32, unit_price: f64) -> Item {
+        Item {
+            sku: sku.to_string(),
+            name: format!("Item {sku}"),
+            category: category.to_string(),
+            quantity,
+            unit_price,
+        }
+    }
+
+    #[test]
+    fn test_summarize_item() {
+        let item = item("A1", "books", 2, 10.0);
+        let summary = CategorySummary::summarize_item(&item, &default_config());
+        assert_eq!(summary.display_name, "Books");
+        assert_eq!(summary.total_value, 20.0);
+    }
+
+    #[test]
+    fn test_empty_report() {
+        let report = Report::generate(&Inventory::new(Vec::new()), &default_config());
+        assert_eq!(report.item_count, 0);
+        assert!(report.categories.is_empty());
+    }
+
+    #[test]
+    fn test_generate_groups_categories() {
+        let inventory = Inventory::new(vec![
+            item("A1", "books", 2, 10.0),
+            item("A2", "books", 3, 12.0),
+            item("A3", "home", 1, 5.0),
+        ]);
+        let report = Report::generate(&inventory, &default_config());
+        assert_eq!(report.categories.len(), 2);
+        assert_eq!(report.categories[0].item_count, 2);
+    }
+
+    #[test]
+    fn test_generate_totals_inventory() {
+        let inventory = Inventory::new(vec![
+            item("A1", "books", 2, 10.0),
+            item("A2", "home", 3, 12.0),
+        ]);
+        assert_eq!(
+            Report::generate(&inventory, &default_config()).total_value,
+            56.0
+        );
+    }
+
+    #[test]
+    fn test_report_serializes_to_json() {
+        let inventory = Inventory::new(vec![item("A1", "books", 2, 10.0)]);
+        let json = serde_json::to_string(&Report::generate(&inventory, &default_config())).unwrap();
+        assert!(json.contains("\"categories\""));
+        assert!(json.contains("\"Books\""));
+    }
+}
diff --git 1/tests/test_benchmark_fixture.py 2/tests/test_benchmark_fixture.py
new file mode 100644
index 0000000..c2763fa
--- /dev/null
+++ 2/tests/test_benchmark_fixture.py
@@ -0,0 +1,202 @@
+"""Regression tests for the reproducible agent-benchmark workspace."""
+
+from __future__ import annotations
+
+import os
+import subprocess
+import shutil
+import sys
+from pathlib import Path
+
+import pytest
+
+
+REPO_ROOT = Path(__file__).resolve().parents[1]
+BUILDER = REPO_ROOT / "benchmarks" / "rebuild_workspace.py"
+EXPECTED_FIXTURE_COMMIT = (
+    REPO_ROOT / "benchmarks" / "workspace-template.commit"
+).read_text().strip()
+CARGO = shutil.which("cargo")
+HAS_CLIPPY = bool(
+    CARGO
+    and subprocess.run(
+        [CARGO, "clippy", "--version"],
+        capture_output=True,
+        timeout=30,
+    ).returncode
+    == 0
+)
+
+
+def run(*args: str, cwd: Path) -> subprocess.CompletedProcess[str]:
+    return subprocess.run(
+        [*args],
+        cwd=cwd,
+        check=True,
+        capture_output=True,
+        text=True,
+        timeout=120,
+    )
+
+
+def test_rebuild_workspace_restores_benchmark_contract(tmp_path):
+    workspace = tmp_path / "workspace"
+    run(
+        sys.executable,
+        str(BUILDER),
+        "--destination",
+        str(workspace),
+        cwd=REPO_ROOT,
+    )
+
+    assert {
+        path.relative_to(workspace).as_posix()
+        for path in (workspace / "src").glob("*.rs")
+    } == {
+        "src/config.rs",
+        "src/inventory.rs",
+        "src/main.rs",
+        "src/report.rs",
+    }
+    assert (workspace / "data" / "inventory.csv").is_file()
+    assert (workspace / "src" / "config.rs").read_text().count("category_labels") == 3
+
+    assert run("git", "rev-list", "--count", "HEAD", cwd=workspace).stdout.strip() == "7"
+    assert (
+        run("git", "rev-parse", "HEAD", cwd=workspace).stdout.strip()
+        == EXPECTED_FIXTURE_COMMIT
+    )
+    assert (
+        run("git", "log", "-1", "--format=%s", cwd=workspace).stdout.strip()
+        == "Initial benchmark workspace"
+    )
+    recent_subjects = run(
+        "git", "log", "-5", "--format=%s", cwd=workspace
+    ).stdout.splitlines()
+    assert "Add inventory data" in recent_subjects
+    assert "Add configuration" in recent_subjects
+    changed_at_head = set(
+        run("git", "diff", "--name-only", "HEAD~1", cwd=workspace).stdout.splitlines()
+    )
+    assert {
+        "src/config.rs",
+        "src/inventory.rs",
+        "src/main.rs",
+        "src/report.rs",
+    } <= changed_at_head
+    assert run("git", "status", "--porcelain", cwd=workspace).stdout == ""
+
+
+@pytest.mark.skipif(CARGO is None, reason="Rust toolchain is not installed")
+def test_rebuilt_workspace_preserves_rust_task_contract(tmp_path):
+    workspace = tmp_path / "workspace"
+    run(
+        sys.executable,
+        str(BUILDER),
+        "--destination",
+        str(workspace),
+        cwd=REPO_ROOT,
+    )
+
+    tests = run(CARGO, "test", "--quiet", "--locked", cwd=workspace)
+    assert "15 passed" in tests.stdout + tests.stderr
+    assert run("git", "status", "--porcelain", cwd=workspace).stdout == ""
+
+
+@pytest.mark.skipif(not HAS_CLIPPY, reason="cargo-clippy is not installed")
+def test_rebuilt_workspace_preserves_clippy_task_contract(tmp_path):
+    workspace = tmp_path / "workspace"
+    run(
+        sys.executable,
+        str(BUILDER),
+        "--destination",
+        str(workspace),
+        cwd=REPO_ROOT,
+    )
+    clippy_target = tmp_path / "clippy-target"
+    clippy = subprocess.run(
+        [CARGO, "clippy", "--quiet", "--locked", "--", "-W", "dead_code"],
+        cwd=workspace,
+        check=True,
+        capture_output=True,
+        text=True,
+        timeout=120,
+        env={**os.environ, "CARGO_TARGET_DIR": str(clippy_target)},
+    )
+    diagnostics = clippy.stdout + clippy.stderr
+    assert "apply_discount" in diagnostics
+    assert "find_by_sku" in diagnostics
+    assert "find_by_category" in diagnostics
+
+
+def test_rebuild_workspace_force_replaces_mutated_fixture(tmp_path):
+    workspace = tmp_path / "workspace"
+    base = [
+        sys.executable,
+        str(BUILDER),
+        "--destination",
+        str(workspace),
+    ]
+    run(*base, cwd=REPO_ROOT)
+    initial_commit = run("git", "rev-parse", "HEAD", cwd=workspace).stdout
+    (workspace / "src" / "config.rs").write_text("broken\n")
+
+    run(*base, "--force", cwd=REPO_ROOT)
+
+    assert "category_labels" in (workspace / "src" / "config.rs").read_text()
+    assert run("git", "rev-parse", "HEAD", cwd=workspace).stdout == initial_commit
+    assert run("git", "status", "--porcelain", cwd=workspace).stdout == ""
+
+
+def test_rebuild_workspace_ignores_inherited_git_identity(tmp_path):
+    workspace = tmp_path / "workspace"
+    env = {
+        **os.environ,
+        "GIT_AUTHOR_NAME": "Unexpected Author",
+        "GIT_AUTHOR_EMAIL": "unexpected@example.com",
+        "GIT_COMMITTER_NAME": "Unexpected Committer",
+        "GIT_COMMITTER_EMAIL": "unexpected@example.com",
+    }
+    subprocess.run(
+        [
+            sys.executable,
+            str(BUILDER),
+            "--destination",
+            str(workspace),
+        ],
+        cwd=REPO_ROOT,
+        check=True,
+        capture_output=True,
+        text=True,
+        timeout=120,
+        env=env,
+    )
+    assert (
+        run("git", "rev-parse", "HEAD", cwd=workspace).stdout.strip()
+        == EXPECTED_FIXTURE_COMMIT
+    )
+
+
+def test_rebuild_workspace_force_refuses_non_fixture_directory(tmp_path):
+    destination = tmp_path / "important"
+    destination.mkdir()
+    preserved = destination / "keep.txt"
+    preserved.write_text("do not delete\n")
+
+    result = subprocess.run(
+        [
+            sys.executable,
+            str(BUILDER),
+            "--destination",
+            str(destination),
+            "--force",
+        ],
+        cwd=REPO_ROOT,
+        capture_output=True,
+        text=True,
+        timeout=120,
+    )
+
+    assert result.returncode != 0
+    assert "refusing to replace" in result.stderr
+    assert preserved.read_text() == "do not delete\n"

### New Points
R1. Suggestions S1-S4 were also adopted: junk patterns are ignored, task history assumptions are tested/documented, the fingerprint has one tracked source, and the builder verifies its generated SHA.
R2. The revised focused test suite passes: 6 passed.

**Requesting:** RE-REVIEW

<!-- event id="review-2" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S5. validate_existing_fixture exempts the default benchmarks/workspace path from the sentinel check, so a user who has replaced that directory with unrelated content can still lose it to --force. Since the default path is the documented fixture location this is an acceptable tradeoff, but consider applying the same sentinel check there too (the generated fixture will always contain the sentinel), removing the exemption entirely.
S6. The sentinel file .daimonos-benchmark-fixture is now committed inside the fixture's own Git history, meaning agents operating in the workspace can see it. If any benchmark task counts files or lists the tree, note this in the README as an intentional, stable artifact of the new lineage so future task authors do not treat it as noise.
S7. rust-toolchain.toml pins 1.96.0, which means cargo commands will attempt a toolchain download on machines without it (rustup) or fail on non-rustup installs; consider documenting this in benchmarks/README.md next to the quick-start so first-time rebuilders are not surprised by a large toolchain fetch during 'cargo test'.

### Responses to Prior Points
Re R1: Confirmed. S1 is addressed by the expanded fixture .gitignore (.DS_Store, *.swp, *~); S2 by the README audit note covering tasks 06/09/11 and the new history-shape assertions (recent subjects and HEAD~1 changed-file set); S3 by the single tracked benchmarks/workspace-template.commit file consumed by both the builder and the test, with the README pointing to it instead of embedding a SHA; S4 by the builder's self-verification that fails loudly on fingerprint mismatch, which will surface environment-induced nondeterminism at rebuild time. All four suggestions are satisfactorily adopted.
Re R2: Acknowledged. The reported 6-passed run is consistent with the revised test file: four unconditional tests plus two toolchain-gated tests that evidently ran (cargo/clippy available). The new negative tests (non-fixture --force refusal, inherited GIT_* identity immunity) directly exercise the two riskiest behaviors, which is exactly the coverage the round-1 questions asked about.
B1: resolved
B2: resolved
B3: resolved

**Verdict:** APPROVE
