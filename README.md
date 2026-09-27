# devknx

`devknx` is a Rust application for observing and operating KNX installations.
It will provide a persistent capture daemon and CLI, TUI, native GUI, REST, and
MCP interfaces over one operation model. It uses `knx-rs-core` and `knx-rs-ip`
for KNX protocol handling.

The repository is in its initial local bootstrap stage. KNXnet/IP discovery is
available from the CLI and a native GUI shell. Monitoring, ETS import,
DPT-aware sending, the TUI, REST, MCP, and release packages are planned, not
yet available.
See [the initiative plan](docs/plans/devknx.md) for the delivery criteria.

```sh
cargo run --locked -- discover
cargo run --locked -- gui
```

On macOS, `scripts/build-macos-app.sh` creates an unsigned local
`dist/devknx.app`. The release pipeline will sign and notarize the application.
The icon source is `resources/icon-master.png`; `scripts/build-icons.sh`
generates the `.icns`, Windows `.ico`, and Linux PNG variants from it on macOS.

The initial release targets macOS ARM64, Linux x86_64 and ARM64, and Windows
x86_64 and ARM64. Linux has both glibc and musl archives. Native GUI support is
qualified for glibc Linux; musl archives are for headless use.

License: GPL-3.0-only.
