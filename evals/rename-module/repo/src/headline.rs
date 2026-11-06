use crate::util::title_case;

pub fn headline(text: &str) -> String {
    format!("{}!", crate::util::title_case(&title_case(text)))
}
