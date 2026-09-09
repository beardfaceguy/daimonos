mod config;
mod inventory;
mod report;

use config::default_config;
use inventory::Inventory;
use report::Report;
use std::fs::File;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = default_config();
    let inventory = Inventory::from_csv(File::open("data/inventory.csv")?)?;

    let _category_count = inventory.items_by_category().len();
    let report = Report::generate(&inventory, &config);
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
