use crate::invoice::Invoice;

/// One `name xquantity: $d.cc` line per item, then `Total: $d.cc`.
pub fn render(invoice: &Invoice) -> String {
    let mut out = String::new();
    for item in &invoice.items {
        out.push_str(&format!("{} x{}: ${:.2}\n", item.name, item.quantity, item.price));
    }
    out.push_str(&format!("Total: ${:.2}\n", invoice.total()));
    out
}
