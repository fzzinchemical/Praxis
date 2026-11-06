use billing::invoice::{Invoice, LineItem};
use billing::report::render;

fn item(name: &str, price_cents: u64, quantity: u32) -> LineItem {
    LineItem { name: name.to_string(), price_cents, quantity }
}

#[test]
fn totals_are_exact_integers() {
    let invoice = Invoice { items: vec![item("a", 10, 1), item("b", 20, 1), item("c", 1999, 3)] };
    assert_eq!(invoice.total_cents(), 6027);
}

#[test]
fn report_format_is_unchanged() {
    let invoice = Invoice { items: vec![item("pen", 105, 2), item("ink", 5, 1)] };
    assert_eq!(render(&invoice), "pen x2: $1.05\nink x1: $0.05\nTotal: $2.15\n");
}

#[test]
fn empty_invoice_totals_zero() {
    assert_eq!(render(&Invoice::default()), "Total: $0.00\n");
}
