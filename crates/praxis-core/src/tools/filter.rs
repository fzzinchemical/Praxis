//! Rule-based shrinking of `Bash` output before the model sees it (SPEC §7, T2.11).
//! It only removes noise: exit codes, errors, warnings, failing tests and stack traces always
//! pass through, and the caller spools the full text.

/// Removes ANSI escapes, keeps only the last state of each carriage-return line, drops
/// per-test pass lines when the run has a summary and `make` directory notes, and counts runs
/// of identical noise lines.
pub fn filter(text: &str) -> String {
    let summarised = text.contains("test result:");
    let mut lines: Vec<(String, usize)> = Vec::new();
    for line in text.lines().map(final_state) {
        if is_noise(&line, summarised) {
            continue;
        }
        match lines.last_mut() {
            Some((previous, count)) if *previous == line && !is_keep(&line) => *count += 1,
            _ => lines.push((line, 1)),
        }
    }
    let mut out = String::new();
    for (line, count) in lines {
        out.push_str(&line);
        out.push('\n');
        if count > 1 {
            out.push_str(&format!(
                "[previous line repeated {} more times]\n",
                count - 1
            ));
        }
    }
    if !text.ends_with('\n') {
        out.pop();
    }
    out
}

/// Strips escape sequences, then keeps what a terminal would show after the last `\r`.
fn final_state(line: &str) -> String {
    let mut plain = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.next() == Some('[') {
            // Only CSI sequences occur in build tool output; they end at a letter.
            for end in chars.by_ref() {
                if end.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(c);
        }
    }
    plain
        .rsplit('\r')
        .find(|part| !part.is_empty())
        .unwrap_or("")
        .to_owned()
}

fn is_noise(line: &str, summarised: bool) -> bool {
    let passed_test = line.starts_with("test ") && line.ends_with(" ... ok");
    let make_dir = line.starts_with("make[")
        && (line.contains("Entering directory") || line.contains("Leaving directory"));
    (summarised && passed_test) || make_dir
}

/// Errors, traces and similar lines are never folded into a repeat count.
fn is_keep(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "error", "warn", "fail", "panic", "trace", "exit", " at ", "::",
    ]
    .iter()
    .any(|word| lower.contains(word))
}
