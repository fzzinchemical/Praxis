/// Uppercases the first letter of every word.
pub fn title_case(text: &str) -> String {
    text.split(' ').map(capitalize).collect::<Vec<_>>().join(" ")
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}
