use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Item {
    pub sku: String,
    pub name: String,
    pub category: String,
    pub quantity: u32,
    pub unit_price: f64,
}

impl Item {
    pub fn is_low_stock(&self, threshold: u32) -> bool {
        self.quantity <= threshold
    }

    pub fn total_value(&self) -> f64 {
        self.quantity as f64 * self.unit_price
    }

    pub fn apply_discount(&mut self, percent: f64) {
        self.unit_price *= 1.0 - percent / 100.0;
    }
}

#[derive(Debug, Default)]
pub struct Inventory {
    items: Vec<Item>,
}

impl Inventory {
    pub fn new(items: Vec<Item>) -> Self {
        Self { items }
    }

    pub fn from_csv(reader: impl Read) -> Result<Self, csv::Error> {
        let mut csv_reader = csv::Reader::from_reader(reader);
        let items = csv_reader.deserialize().collect::<Result<Vec<_>, _>>()?;
        Ok(Self::new(items))
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn find_by_sku(&self, sku: &str) -> Option<&Item> {
        self.items.iter().find(|item| item.sku == sku)
    }

    pub fn find_by_category(&self, category: &str) -> Vec<&Item> {
        self.items
            .iter()
            .filter(|item| item.category == category)
            .collect()
    }

    pub fn low_stock_items(&self, threshold: u32) -> Vec<&Item> {
        self.items
            .iter()
            .filter(|item| item.is_low_stock(threshold))
            .collect()
    }

    pub fn total_inventory_value(&self) -> f64 {
        self.items.iter().map(Item::total_value).sum()
    }

    pub fn items_by_category(&self) -> HashMap<&str, Vec<&Item>> {
        let mut grouped = HashMap::new();
        for item in &self.items {
            grouped
                .entry(item.category.as_str())
                .or_insert_with(Vec::new)
                .push(item);
        }
        grouped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_new_inventory() {
        assert!(Inventory::new(Vec::new()).items().is_empty());
    }

    #[test]
    fn test_from_csv() {
        let input = "sku,name,category,quantity,unit_price\nA1,Mouse,electronics,2,25.0\n";
        let inventory = Inventory::from_csv(input.as_bytes()).unwrap();
        assert_eq!(inventory.items().len(), 1);
        assert_eq!(inventory.items()[0].sku, "A1");
    }

    #[test]
    fn test_find_by_sku() {
        let inventory = Inventory::new(vec![item("A1", "books", 2, 10.0)]);
        assert_eq!(inventory.find_by_sku("A1").unwrap().name, "Item A1");
        assert!(inventory.find_by_sku("missing").is_none());
    }

    #[test]
    fn test_find_by_category() {
        let inventory = Inventory::new(vec![
            item("A1", "books", 2, 10.0),
            item("A2", "home", 3, 12.0),
        ]);
        assert_eq!(inventory.find_by_category("books").len(), 1);
    }

    #[test]
    fn test_low_stock_items() {
        let inventory = Inventory::new(vec![
            item("A1", "books", 2, 10.0),
            item("A2", "books", 8, 12.0),
        ]);
        assert_eq!(inventory.low_stock_items(5).len(), 1);
    }

    #[test]
    fn test_total_value() {
        assert_eq!(item("A1", "books", 3, 12.5).total_value(), 37.5);
    }

    #[test]
    fn test_total_inventory_value() {
        let inventory = Inventory::new(vec![
            item("A1", "books", 2, 10.0),
            item("A2", "home", 3, 12.0),
        ]);
        assert_eq!(inventory.total_inventory_value(), 56.0);
    }
}
