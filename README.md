<p align="center">
  <img src="resources/png/devknx-128.png" width="104" height="104" alt="devknx app icon">
</p>

<h1 align="center">devknx</h1>

<p align="center">
  A KNX monitor and control application written in Rust.
  <br>
  One connection-independent daemon for the terminal, desktop, REST, and MCP.
</p>

<p align="center">
  <a href="https://github.com/metaneutrons/devknx/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/metaneutrons/devknx/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="GPL-3.0-only" src="https://img.shields.io/badge/license-GPL--3.0--only-blue.svg"></a>
  <a href="https://github.com/metaneutrons/devknx/issues"><img alt="GitHub issues" src="https://img.shields.io/github/issues/metaneutrons/devknx"></a>
</p>

> [!IMPORTANT]
> **Early development.** There is no stable release yet. The daemon can run without a KNX connection. GUI and TUI connect only when requested. Group writes can affect a live installation; qualify them on an isolated test network first.

## What devknx is building

devknx combines the KNX protocol implementation in the
[`knx-rs` crate family](https://github.com/metaneutrons/knx-rs) with a
persistent local capture service. A single typed operation model will serve
every interface, so a DPT validation rule cannot silently differ between a
desktop button and an API call.

| Capability | Current state |
| --- | --- |
| KNXnet/IP gateway discovery | CLI, TUI, and native GUI available |
| KNXnet/IP tunneling and routing capture | Experimental CLI stream with bounded reconnect and endpoint-specific SQLite persistence |
| Independent capture process | One per-user daemon with explicit endpoint sessions, current-user control IPC, and endpoint-scoped capture streams |
| Durable telegram history | Experimental SQLite history with ID cursor and retention cap; interactive views filter the loaded window and export the full retained history |
| ETS group-address CSV and XML import | Experimental CLI import; TUI and GUI display active ETS labels and DPT declarations |
| DPT-validated read, write preview, and write | Experimental CLI/TUI/GUI operations through the single capture owner; loopback-qualified, not yet hardware-qualified |
| Terminal UI and native desktop UI | Experimental; the connection workflow and toolbar have been revised after visual feedback and require renewed visual qualification |
| Local REST and MCP interfaces | Experimental daemon-controlled REST listener and structured MCP stdio tools; both use the same DPT-validated operation path |

ETS group-address metadata is stored in separate revisions without changing
raw captured telegrams. A typed write requires an unambiguous DPT (from ETS or
explicitly supplied), and its preview is the exact frame sent by the capture
owner. A read reports transmission separately from a matching response or
timeout; transmission alone does not establish an actuator state change.

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
cargo run --locked -- gui
cargo run --locked -- tui --endpoint tunnel://192.0.2.1:3671
```

Gateway discovery sends KNXnet/IP multicast on the local network. Network
equipment and host firewall rules can affect the result. For normal interactive
use, launch the GUI and select a connection, or start the TUI with an explicit
`--endpoint`. Neither creates an unassigned global capture database.
In the GUI, choose Tunneling or Routing on the start screen, enter the
unicast gateway IP or multicast group and UDP port, then select Connect.
Settings… uses the same connection form. Gateway discovery is optional for a
manually entered tunnel address. In the TUI,
press `s` to change the endpoint, then `c` to connect or disconnect. A running
capture service remains active when an interactive window closes; use
Disconnect or `c` to stop it. An explicit `--database` fixes the capture path
instead of deriving it from the endpoint.
The GUI opens its own capture history automatically. Use **Open…** (or
**File > Open Capture…** on macOS) to choose another saved SQLite capture in
the system file dialog; **Live capture** returns to the active history. A
pre-connection `captures.sqlite` is offered as a previous capture, not
automatically attributed to a gateway.

For headless use, `daemon` runs without connecting to KNX until a session is
requested. `connect` and endpoint-selected live operations start it on demand.
Run `daemon` in the foreground under a service manager if desired. Sessions
remain active after a CLI monitor, TUI, or GUI closes, until explicitly
disconnected or the daemon stops. Select a connection's default database with
`--endpoint`, or an existing capture with `--database`:

```sh
devknx daemon --status
devknx connect tunnel://192.0.2.1:3671
devknx sessions
devknx status --endpoint tunnel://192.0.2.1:3671
devknx history --endpoint tunnel://192.0.2.1:3671 --after 0 --limit 100
devknx follow --endpoint tunnel://192.0.2.1:3671
devknx rest --enable --endpoint tunnel://192.0.2.1:3671
devknx rest --status
devknx mcp --endpoint tunnel://192.0.2.1:3671
devknx write-preview --endpoint tunnel://192.0.2.1:3671 1/2/3 true
devknx read --endpoint tunnel://192.0.2.1:3671 1/2/3
devknx write --endpoint tunnel://192.0.2.1:3671 1/2/3 true
devknx rest --disable
devknx disconnect tunnel://192.0.2.1:3671
devknx daemon --stop
```

The read and write lines are examples, not a script to run against an unknown
installation. `follow` and `mcp` remain attached until stopped. `read` and
`write` with `--endpoint` start the daemon and connect that specific endpoint
if needed; `--database` alone never guesses an endpoint or starts a session.
`history`, `export`, `write-preview`, and `daemon --status` do not connect.
`write-preview` does not transmit. The CLI never uses the GUI's remembered
connection as a write target.

The daemon owns one opt-in REST listener, bound to `127.0.0.1:8765` by
default and scoped to one connected endpoint. The GUI controls it in
its own REST API dialog, opened from the live status in the bottom bar.
The TUI's `a` key toggles a loopback listener after confirmation. The CLI
also exposes the bind address, bearer token and remote-write policy. See the
[REST API guide](docs/rest.md) for versioned routes,
SSE resume, authentication and remote-write policy.
The MCP adapter is a separate local stdio process; see the [MCP guide](docs/mcp.md)
for tool names, structured results, bounded search and write semantics.

The GUI shows a bounded live/history view with readable local timestamps,
ETS names and unambiguous DPT-decoded values, a text filter, raw cEMI details,
read and prepared typed-write dialogs, and non-overwriting CSV export. Its
separate toolbar exposes connection and capture actions; capture storage is
explained under Settings rather than presented as the KNX connection. Its macOS
app has native application, File, Edit, View, Operation, Window and Help menus.
The TUI offers `c` connect/disconnect, `s` endpoint settings, `a` REST loopback control, `/` filter,
`r` read, `w` prepared typed write, `e` export, `h` reload, `PgUp` older history, `d` discovery,
`j`/`k` scroll and `q` quit.
In both interfaces, a typed write is previewed before a separate send action.
Expert raw sending, ETS import, backup and durable audit inspection remain CLI
commands. The [capability registry](src/capabilities.rs) records these explicit
differences. On attach the interactive views load the latest 1,000 rows and
can page backward into older retained history. They keep up to 5,000 rows in
memory; a text filter narrows that window, not
the entire database. `history --after ... --limit ... --filter ...` filters one
bounded CLI page. CSV export still covers the complete retained capture history.

`monitor` attaches to a managed session and prints one line per cEMI frame, including a millisecond
timestamp, endpoint, source and destination addresses, group-value service,
and the exact raw frame in hexadecimal. Connection changes are printed to
standard error. Failed connection attempts and unexpected closes are retried
with a bounded 1–30 second delay; press Ctrl-C to detach while capture
continues. The session also captures its own sent operation frames. A router's KNXnet/IP
`RoutingLostMessage` diagnostic is stored and streamed separately from cEMI
frames. It includes the reporting router, device state, and count of routing
frames the router says it lost. A slow local subscriber is reported separately
as application-event lag; a connection interruption has no inferred loss count.
None is a general KNX bus-loss total.
The endpoints above are examples; substitute your own network addresses.
Passive receive, restart, local IPC, and SQLite backup have been exercised
against a real KNXnet/IP tunnel on macOS ARM64 and a physical multicast router
on Linux x86_64. The observed bus traffic contained group writes, not reads or
responses. No real router loss report has been observed in the qualification
window; parser and integration tests exercise the diagnostic path. See the
[M2 evidence and limits](https://github.com/metaneutrons/devknx/issues/5).

`connect`, `monitor`, GUI Connect, and TUI Connect create a versioned SQLite
capture store for the selected endpoint. By default, its private per-user path
encodes the canonical mode, IP address, and port; `--database` overrides it.
Each
committed telegram receives a monotonic ID; the default retention limit is
100,000 frames and can be changed with `--max-events`. `history` reads an
existing store using an exclusive `--after` ID and a bounded page size.
`export` streams CSV up to the highest ID present when export begins. `backup`
creates a consistent SQLite snapshot, including committed WAL transactions
while a session is running; it never overwrites an existing destination. All
three commands fail rather than create an empty database if the source path is
wrong.
`router-losses` reads a separate, bounded diagnostic history with its own
monotonic ID cursor; it does not mix router reports with cEMI history or CSV.
Opening a version-one or version-two database for writing migrates it transactionally to
schema version five; read-only history remains available during migration.
New managed sessions bind their database to one canonical endpoint, including
when a custom path is used. A different endpoint cannot silently reuse it;
the check runs before any retention pruning.
Existing databases with an unsupported schema are not rewritten. The daemon
owns independent capture services and live event buses for each selected
endpoint. Each service has one database writer and continues capture when a
monitor subscriber exits or falls behind. A service manager may keep the
daemon alive; it does not choose which KNX endpoints connect.
`status`, `follow`, `read`, and `write` connect to the running owner over current-user local IPC;
`follow` emits development-version-three JSON lines with connection states, committed captures,
router reports, and explicit per-stream application-subscriber lag counts. The
GUI, TUI and CLI use a separate current-user control endpoint to connect or
disconnect sessions without stopping the daemon. One writable owner
per database is enforced by a sidecar `.writer.lock` file, which is retained
across restarts and must not be deleted while capture runs. On Unix, a writable
capture directory must not be group- or world-writable. The Unix IPC socket
normally lives under a private directory beside the database, with a short
private `/tmp` fallback for Unix socket path limits; the Windows named pipe
uses a current-user access-control list. The GUI and TUI attach to this stream
and reconnect after an owner restart.
The [local IPC protocol](docs/local-ipc.md) is documented for development
clients; it is not yet a stable external API.

ETS imports are bounded, validated, and all-or-nothing; they create a new
metadata revision without modifying raw captures. Standard four-column ETS
CSV 3/1 has no DPT declaration or description. GA Export 01 XML can include
both, and all declared DPTs are retained rather than selecting the first.
See [ETS import and safety rules](docs/ets-import.md). Disconnect the selected
session before an import: its single-writer lease also protects metadata updates.

`write-preview` performs no network action. `write` resolves ETS declarations
again inside the capture owner before sending; an absent or ambiguous DPT needs
`--dpt`, and an explicit DPT cannot contradict the imported declaration.
Unsupported DPT identifiers and unparseable values fail before sending; the
currently supported typed identifiers are listed in [ETS import and safety
rules](docs/ets-import.md). `write-raw` is a
separate expert command with an explicit `--inline` or `--bytes` payload. Every
accepted attempt receives a durable `audit` record before transmission,
identifying `typed_write`, `raw_write`, or `read`; an interrupted attempt remains
`started` rather than appearing successful. `read` reports a matching
group-value response or `no_response` after a bounded timeout. These paths
have loopback tests only; no production write qualification has been performed.

Run `daemon` under a service manager if desired. Starting the daemon alone
does not touch KNX. `monitor` may attach to an already active session without
competing for its database writer. `disconnect` ends only the named session;
`daemon --stop` ends every session and the daemon.

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

No package listed here is available yet. The non-publishing
[release-candidate build](docs/release-preparation.md) produces temporary CI
artifacts for packaging tests; it is not a release. Release qualification will
cover:

| Platform | Architectures | Deliverables |
| --- | --- | --- |
| macOS | ARM64 only | CLI archive, notarized `.app.zip`, Homebrew `devknx` CLI formula and separate `devknx-app` cask |
| Linux | x86_64 and ARM64 | GNU and musl CLI archives, Debian packages, Homebrew formula, AUR packages |
| Windows | x86_64 and ARM64 | CLI archives with the icon embedded in each executable |

Windows binaries are not Authenticode-signed. SmartScreen may warn on first
launch; verify the release's checksum, cosign bundle and provenance before
running a downloaded binary.

The Homebrew formula will put `devknx` on `PATH`; the cask will put
`devknx.app` in `/Applications`. They will install side by side. The packages
will be published through GitHub Releases, the
[`metaneutrons` Homebrew tap](https://github.com/metaneutrons/homebrew-tap),
the AUR, and the shared [`deb.metaneutrons.cc`](https://deb.metaneutrons.cc/index.html)
archive. The macOS Intel target is intentionally excluded. Linux musl archives
are headless; native GUI qualification is for GNU/glibc Linux.

The [versioned initiative plan](docs/plans/devknx.md) defines the architecture,
milestones, acceptance evidence, and the later KnxMonitor handoff. An
[M2 real-hardware qualification procedure](docs/qualification/m2-hardware.md)
defines the live method; measured results and limits are recorded in the
[M2 issue](https://github.com/metaneutrons/devknx/issues/5). The project will appear on
[metaneutrons.cc](https://metaneutrons.cc), the overview of
Fabian's repositories; it does not require a separate project website.
The [capability registry](src/capabilities.rs) records which current operations
exist in the CLI, TUI and GUI and why their coverage differs during development.
The [M4 qualification record](https://github.com/metaneutrons/devknx/issues/7)
links the native GUI live/reconnect CI run and the TUI 5,000-row test. These
tests do not replace a visual review of the app before the first stable release.

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
