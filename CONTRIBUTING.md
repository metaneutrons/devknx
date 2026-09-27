# Contributing

Use English for code, documentation, issues, pull requests, and commit messages.
Commit messages and pull request titles follow Conventional Commits.

Before submitting a change, run:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Substantial work is tracked against [the initiative plan](docs/plans/devknx.md).
Its acceptance criteria are authoritative; issues record implementation state.
