pub struct LineItem {
    pub name: String,
    pub price: f64,
    pub quantity: u32,
}

#[derive(Default)]
pub struct Invoice {
    pub items: Vec<LineItem>,
}

impl Invoice {
    pub fn total(&self) -> f64 {
        self.items.iter().map(|i| i.price * f64::from(i.quantity)).sum()
    }
}
