use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: wordcount <file>");
        return ExitCode::from(2);
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            println!("{} words", text.split_whitespace().count());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{path}: {error}");
            ExitCode::FAILURE
        }
    }
}
