# Contributing

Use English for code, documentation, issues, pull requests, and commit messages.
Commit messages and pull request titles follow Conventional Commits. Name
branches by purpose, for example `feat/ets-import` or `fix/discovery-timeout`.

Install the repository hooks once after cloning:

```sh
lefthook install
```

The hooks require `lefthook` and `gitleaks`; CI repeats their checks. The
initial repository bootstrap was the only direct commit to `main`. Submit
subsequent work through a pull request.

Before submitting a change, run:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo nextest run --all-features --locked
cargo deny check
```

Substantial work is tracked against [the initiative plan](docs/plans/devknx.md).
Its acceptance criteria are authoritative; issues record implementation state.
Do not include real ETS exports, bus captures, gateway credentials, or building
metadata in an issue or a test fixture.

When adding or removing a user-facing CLI or GUI operation, update the
[capability registry](src/capabilities.rs), its source anchor, and any
intentional cross-surface gap. The registry test rejects missing anchors and
unrecorded or stale differences. It describes the full build; it cannot infer
which controls conditional compilation excludes from a particular binary.
