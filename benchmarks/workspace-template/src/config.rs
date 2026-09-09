use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub low_stock_threshold: u32,
    category_labels: HashMap<String, String>,
}

impl AppConfig {
    pub fn category_display_name(&self, category: &str) -> String {
        self.category_labels
            .get(category)
            .cloned()
            .unwrap_or_else(|| category.to_string())
    }
}

pub fn default_config() -> AppConfig {
    AppConfig {
        low_stock_threshold: 5,
        category_labels: HashMap::from([
            ("books".to_string(), "Books".to_string()),
            ("electronics".to_string(), "Electronics".to_string()),
            ("home".to_string(), "Home & Garden".to_string()),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_low_stock_threshold() {
        assert_eq!(default_config().low_stock_threshold, 5);
    }

    #[test]
    fn test_known_category_display_name() {
        assert_eq!(
            default_config().category_display_name("electronics"),
            "Electronics"
        );
    }

    #[test]
    fn test_unknown_category_display_name() {
        assert_eq!(default_config().category_display_name("other"), "other");
    }
}
