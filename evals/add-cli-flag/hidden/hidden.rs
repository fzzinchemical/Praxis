use std::path::PathBuf;
use std::process::{Command, Output};

fn sample() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sample.txt");
    std::fs::write(&path, "one two\nthree\n\n").unwrap();
    path
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wordcount")).args(args).output().unwrap()
}

#[test]
fn json_flag_works_in_either_position() {
    let file = sample();
    let file = file.to_str().unwrap();
    for args in [["--json", file], [file, "--json"]] {
        let out = run(&args);
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "{\"words\":3,\"lines\":3}\n");
    }
}

#[test]
fn plain_output_is_unchanged() {
    let file = sample();
    let out = run(&[file.to_str().unwrap()]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "3 words\n");
}

#[test]
fn errors_still_exit_nonzero() {
    assert!(!run(&["/nonexistent/file"]).status.success());
    assert!(!run(&["--json"]).status.success());
    assert!(!run(&[]).status.success());
}
