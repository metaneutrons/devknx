<p align="center">
  <img src="resources/png/devknx-128.png" width="104" height="104" alt="devknx app icon">
</p>

<h1 align="center">devknx</h1>

<p align="center">
  A KNX monitor and control application written in Rust.
  <br>
  One capture engine for the terminal, desktop, REST, and MCP.
</p>

<p align="center">
  <a href="https://github.com/metaneutrons/devknx/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/metaneutrons/devknx/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="GPL-3.0-only" src="https://img.shields.io/badge/license-GPL--3.0--only-blue.svg"></a>
  <a href="https://github.com/metaneutrons/devknx/issues"><img alt="GitHub issues" src="https://img.shields.io/github/issues/metaneutrons/devknx"></a>
</p>

> [!IMPORTANT]
> **Early development.** There is no stable release yet. The CLI can discover gateways, capture raw telegrams from a KNXnet/IP tunnel or router, and retain them in SQLite through a foreground capture process. The native GUI is still a discovery shell. There is no IPC-enabled background daemon, ETS import, or group-value sending. Do not use it to operate a live installation.

## What devknx is building

devknx combines the KNX protocol implementation in the
[`knx-rs` crate family](https://github.com/metaneutrons/knx-rs) with a
persistent local capture service. A single typed operation model will serve
every interface, so a DPT validation rule cannot silently differ between a
desktop button and an API call.

| Capability | Current state |
| --- | --- |
| KNXnet/IP gateway discovery | CLI and native GUI shell available |
| KNXnet/IP tunneling and routing capture | Experimental CLI stream with bounded reconnect and optional SQLite persistence |
| Independent capture process | Experimental foreground `serve` command; no IPC or automatic startup yet |
| Durable, searchable telegram history | Experimental SQLite history with ID cursor, retention cap, and CSV export; filters/search and the background daemon are pending |
| ETS group-address CSV and XML import | Planned |
| DPT-validated read, write preview, and write | Planned |
| Terminal UI and full native desktop UI | Planned |
| Local REST and MCP interfaces | Planned |

Group-address names and DPT declarations from ETS will enrich captured
telegrams without changing their raw frames. A typed write will require an
unambiguous DPT (from ETS or explicitly supplied), show the exact encoded frame
before transmission, and report transmission separately from a device response.

The first stable version targets KNXnet/IP. The older
[`KnxMonitor`](https://github.com/metaneutrons/KnxMonitor) also supports KNX-USB;
**USB is not part of devknx's initial release**. KnxMonitor will remain
available, with this difference stated explicitly when it is eventually
marked deprecated.

## Try the development build

Install [Rust via rustup](https://rustup.rs/), then run:

```sh
git clone https://github.com/metaneutrons/devknx.git
cd devknx
cargo run --locked -- discover
cargo run --locked -- monitor tunnel://192.0.2.1:3671
cargo run --locked -- monitor router://224.0.23.12:3671
cargo run --locked -- monitor tunnel://192.0.2.1:3671 --database captures.sqlite
cargo run --locked -- serve tunnel://192.0.2.1:3671 --database captures.sqlite
cargo run --locked -- history --database captures.sqlite --after 0 --limit 100
cargo run --locked -- export --database captures.sqlite > captures.csv
cargo run --locked -- backup --database captures.sqlite --output captures-backup.sqlite
cargo run --locked -- gui
```

Gateway discovery sends KNXnet/IP multicast on the local network. Network
equipment and host firewall rules can affect the result. The GUI can also be
opened without a gateway; its current purpose is to exercise the native app
shell and discovery view.

`monitor` prints one line per received cEMI frame, including a millisecond
timestamp, endpoint, source and destination addresses, group-value service,
and the exact raw frame in hexadecimal. Connection changes are printed to
standard error. Failed connection attempts and unexpected closes are retried
with a bounded 1–30 second delay; press Ctrl-C to stop. Only receive-side
frames are captured in this development build. A slow live subscriber is
reported as application-event lag, not as a count of lost KNX bus telegrams.
The endpoints above are examples, not verified gateways; substitute your own
network addresses. No live-bus qualification has been performed yet.

The optional `--database` creates a versioned SQLite capture store. Each
committed telegram receives a monotonic ID; the default retention limit is
100,000 frames and can be changed with `--max-events`. `history` reads an
existing store using an exclusive `--after` ID and a bounded page size.
`export` streams CSV up to the highest ID present when export begins. `backup`
creates a consistent SQLite snapshot, including committed WAL transactions
while `serve` is running; it never overwrites an existing destination. All
three commands fail rather than create an empty database if the source path is
wrong.
Existing databases with an unsupported schema are not rewritten. The CLI now
runs an in-process connection owner and live event bus; capture continues if
its terminal subscriber falls behind. The connection and database are still
owned only while `monitor` is running. `serve` runs the same capture owner in a
separate foreground process; it can be kept alive by a service manager while
`history` and `export` read the database from other processes. It does not
detach, auto-start, or provide live IPC to other frontends. One writable owner
per database is enforced by a sidecar `.writer.lock` file, which is retained
across restarts and must not be deleted while capture runs. On Unix, a writable
capture directory must not be group- or world-writable. An IPC-enabled daemon
and cross-process live subscription remain planned.

Run `serve` under a service manager if capture must survive terminal closure;
use another terminal for `history` or `export` while it runs.

Capture files can reveal activity in a building. Newly created SQLite files
are private to the current user on Unix; choose a protected directory and
apply suitable access controls for existing files and on Windows.

For a headless CLI build:

```sh
cargo build --locked --no-default-features
```

On macOS ARM64, `scripts/build-macos-app.sh` (requires `jq`) assembles an **unsigned local**
`dist/devknx.app`. The release bundle will be signed, notarized, and distributed
separately. `resources/icon-master.png` is the source for the macOS `.icns`,
Windows executable `.ico`, and Linux icon sizes. Embedded font licences are
listed in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

## Planned distribution

No package listed here is available yet. Release qualification will cover:

| Platform | Architectures | Deliverables |
| --- | --- | --- |
| macOS | ARM64 only | CLI archive, notarized `.app.zip`, Homebrew formula and cask |
| Linux | x86_64 and ARM64 | GNU and musl CLI archives, Debian packages, Homebrew formula, AUR packages |
| Windows | x86_64 and ARM64 | CLI archives with the icon embedded in each executable |

The packages will be published through GitHub Releases, the
[`metaneutrons` Homebrew tap](https://github.com/metaneutrons/homebrew-tap),
the AUR, and the shared [`deb.metaneutrons.cc`](https://deb.metaneutrons.cc/index.html)
archive. The macOS Intel target is intentionally excluded. Linux musl archives
are headless; native GUI qualification is for GNU/glibc Linux.

The [versioned initiative plan](docs/plans/devknx.md) defines the architecture,
milestones, acceptance evidence, and the later KnxMonitor handoff. The project
will also appear on [metaneutrons.cc](https://metaneutrons.cc), the overview of
Fabian's repositories; it does not require a separate project website.
The [capability registry](src/capabilities.rs) records which current operations
exist in the CLI and GUI and why their coverage differs during development.

## Development and security

The repository pins its Rust toolchain and commits `Cargo.lock`. Before a pull
request, run:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --all-features --locked
cargo deny check
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the commit convention and local hooks.
Report vulnerabilities through [private vulnerability reporting](https://github.com/metaneutrons/devknx/security/advisories/new), not a public issue.

devknx is licensed under [GPL-3.0-only](LICENSE).
