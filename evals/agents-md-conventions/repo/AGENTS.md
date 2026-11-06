# Rules for this repo

- Fallible functions return `Result<_, crate::Error>`. Bad input is `Error::Invalid` with the offending text.
- Every public function has a `///` doc comment directly above it.
- Library code never uses `unwrap`, `expect` or `panic!`.
- Every user-visible change gets a bullet under `## Unreleased` in `CHANGELOG.md` naming the function.
