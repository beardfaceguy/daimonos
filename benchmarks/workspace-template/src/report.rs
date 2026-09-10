use crate::config::AppConfig;
use crate::inventory::{Inventory, Item};
use serde::Serialize;

#[derive(Debug, PartialEq, Serialize)]
pub struct CategorySummary {
    pub category: String,
    pub display_name: String,
    pub item_count: usize,
    pub quantity: u32,
    pub total_value: f64,
}

impl CategorySummary {
    fn summarize_item(item: &Item, config: &AppConfig) -> Self {
        Self {
            category: item.category.clone(),
            display_name: config.category_display_name(&item.category),
            item_count: 1,
            quantity: item.quantity,
            total_value: item.total_value(),
        }
    }
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Report {
    pub item_count: usize,
    pub total_value: f64,
    pub low_stock_count: usize,
    pub categories: Vec<CategorySummary>,
}

impl Report {
    pub fn generate(inventory: &Inventory, config: &AppConfig) -> Self {
        let mut categories = Vec::<CategorySummary>::new();
        for item in inventory.items() {
            if let Some(summary) = categories
                .iter_mut()
                .find(|summary| summary.category == item.category)
            {
                summary.item_count += 1;
                summary.quantity += item.quantity;
                summary.total_value += item.total_value();
            } else {
                categories.push(CategorySummary::summarize_item(item, config));
            }
        }
        categories.sort_by(|left, right| left.category.cmp(&right.category));

        Self {
            item_count: inventory.items().len(),
            total_value: inventory.total_inventory_value(),
            low_stock_count: inventory.low_stock_items(config.low_stock_threshold).len(),
            categories,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_config;

    fn item(sku: &str, category: &str, quantity: u32, unit_price: f64) -> Item {
        Item {
            sku: sku.to_string(),
            name: format!("Item {sku}"),
            category: category.to_string(),
            quantity,
            unit_price,
        }
    }

    #[test]
    fn test_summarize_item() {
        let item = item("A1", "books", 2, 10.0);
        let summary = CategorySummary::summarize_item(&item, &default_config());
        assert_eq!(summary.display_name, "Books");
        assert_eq!(summary.total_value, 20.0);
    }

    #[test]
    fn test_empty_report() {
        let report = Report::generate(&Inventory::new(Vec::new()), &default_config());
        assert_eq!(report.item_count, 0);
        assert!(report.categories.is_empty());
    }

    #[test]
    fn test_generate_groups_categories() {
        let inventory = Inventory::new(vec![
            item("A1", "books", 2, 10.0),
            item("A2", "books", 3, 12.0),
            item("A3", "home", 1, 5.0),
        ]);
        let report = Report::generate(&inventory, &default_config());
        assert_eq!(report.categories.len(), 2);
        assert_eq!(report.categories[0].item_count, 2);
    }

    #[test]
    fn test_generate_totals_inventory() {
        let inventory = Inventory::new(vec![
            item("A1", "books", 2, 10.0),
            item("A2", "home", 3, 12.0),
        ]);
        assert_eq!(
            Report::generate(&inventory, &default_config()).total_value,
            56.0
        );
    }

    #[test]
    fn test_report_serializes_to_json() {
        let inventory = Inventory::new(vec![item("A1", "books", 2, 10.0)]);
        let json = serde_json::to_string(&Report::generate(&inventory, &default_config())).unwrap();
        assert!(json.contains("\"categories\""));
        assert!(json.contains("\"Books\""));
    }
}
