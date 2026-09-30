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
  <a href="https://github.com/metaneutrons/devknx/releases/latest"><img alt="GitHub release" src="https://img.shields.io/github/v/release/metaneutrons/devknx"></a>
  <a href="LICENSE"><img alt="GPL-3.0-only" src="https://img.shields.io/badge/license-GPL--3.0--only-blue.svg"></a>
  <a href="https://github.com/metaneutrons/devknx/issues"><img alt="GitHub issues" src="https://img.shields.io/github/issues/metaneutrons/devknx"></a>
</p>

> [!IMPORTANT]
> **Initial release: 0.1.0.** The daemon can run without a KNX connection. GUI and TUI connect only when requested. Group writes can affect a live installation; qualify them on an isolated test network first. Live receive has been hardware-tested; typed read/write operations have loopback tests, not production hardware qualification.

## What devknx does

devknx combines the KNX protocol implementation in the
[`knx-rs` crate family](https://github.com/metaneutrons/knx-rs) with a
persistent local capture service. A single typed operation model serves
every interface, so a DPT validation rule cannot silently differ between a
desktop button and an API call.

| Capability | Current state |
| --- | --- |
| KNXnet/IP gateway discovery | CLI, TUI, and native GUI available |
| KNXnet/IP tunneling and routing capture | Daemon-owned sessions shared by CLI, TUI, GUI, REST, and MCP, with bounded reconnect and endpoint-specific SQLite persistence |
| Independent capture process | One per-user daemon with explicit endpoint sessions, current-user control IPC, and endpoint-scoped capture streams |
| Durable telegram history | Experimental SQLite history with ID cursor and retention cap; interactive views filter the loaded window and export the full retained history |
| ETS group-address CSV and XML import | CLI import and preview-confirmed TUI/GUI import; all surfaces use active ETS labels and DPT declarations |
| DPT-validated read, write preview, and write | Experimental CLI/TUI/GUI operations through the single capture owner; loopback-qualified, not yet hardware-qualified |
| Terminal UI and native desktop UI | Connection settings, full-height capture table, DPT details, ETS import, read/write previews, and export; local macOS visual checks and automated UI tests, not full cross-platform visual qualification |
| Local REST and MCP interfaces | Experimental daemon-controlled REST listener and structured MCP stdio tools; both use the same DPT-validated operation path |

ETS group-address metadata is stored in separate revisions without changing
raw captured telegrams. A typed write requires an unambiguous DPT (from ETS or
explicitly supplied), and its preview is the exact frame sent by the capture
owner. A read reports transmission separately from a matching response or
timeout; transmission alone does not establish an actuator state change.

devknx 0.1.0 targets KNXnet/IP. The older
[`KnxMonitor`](https://github.com/metaneutrons/KnxMonitor) also supports KNX-USB;
**USB is not part of devknx's initial release**. Keep KnxMonitor if you need
KNX-USB; devknx is not yet a replacement for that connection type.

## Installation

Download the appropriate archive from [GitHub Releases](https://github.com/metaneutrons/devknx/releases/latest),
or use a package channel below. The standalone CLI and macOS app can be installed
side by side.

### Homebrew

For the CLI on macOS ARM64 or Linux x86_64/ARM64:

```sh
brew tap metaneutrons/tap
brew install devknx
```

For the signed and notarized macOS ARM64 app (macOS 12 or newer):

```sh
brew install --cask metaneutrons/tap/devknx-app
```

The formula puts `devknx` on `PATH`; the cask installs `devknx.app` in
`/Applications`. Sparkle updates only the app, never the standalone CLI.

### Linux: APT and AUR

For Debian/Ubuntu, follow the fingerprint-checked setup instructions on
[`deb.metaneutrons.cc`](https://deb.metaneutrons.cc/index.html) to add the shared,
signed archive, then install:

```sh
sudo apt-get update
sudo apt-get install devknx
```

For Arch Linux, use [`devknx-bin`](https://aur.archlinux.org/packages/devknx-bin)
for prebuilt binaries or [`devknx`](https://aur.archlinux.org/packages/devknx)
to build from source. Linux musl archives are headless; GNU/glibc packages
include the GUI.

### Windows

Extract the x86_64 or ARM64 ZIP from [GitHub Releases](https://github.com/metaneutrons/devknx/releases/latest).
Run `devknx.exe gui` for the desktop UI or `devknx.exe tui --endpoint
tunnel://YOUR_GATEWAY:3671` for the terminal UI. Add the executable's directory
to `PATH` if you want to use it from any terminal.

Windows binaries are not Authenticode-signed. SmartScreen may warn on first
launch; verify the release's checksum, cosign bundle, and provenance before
running a downloaded binary.

## Try the development build

Install [Rust via rustup](https://rustup.rs/), then run:

```sh
git clone https://github.com/metaneutrons/devknx.git
cd devknx
cargo build --locked
cargo run --locked -- gui
# Alternatively, start the terminal UI with your gateway address:
cargo run --locked -- tui --endpoint tunnel://192.0.2.1:3671
```

The addresses in this README are examples; substitute your own network addresses.
For CLI examples below, use `./target/debug/devknx` or install the development
binary with `cargo install --locked --path .`.

## GUI and TUI

Launch the GUI and select a connection, or start the TUI with an explicit
`--endpoint`. Neither creates an unassigned global capture database.
In the GUI, choose Tunneling or Routing on the start screen, enter the
unicast gateway IP or multicast group and UDP port, then select Connect.
Settings… uses the same connection form. Gateway discovery is optional for a
manually entered tunnel address. Discovery sends KNXnet/IP multicast on the
local network; network equipment and firewall rules can affect the result.
In the TUI,
press `s` to change the endpoint, then `c` to connect or disconnect. A running
capture service remains active when an interactive window closes; use
Disconnect or `c` to stop it. An explicit `--database` fixes the capture path
instead of deriving it from the endpoint.
The GUI opens its own capture history automatically. Use **Open…** (or
**File > Open Capture…** on macOS) to choose another saved SQLite capture in
the system file dialog; **Live capture** returns to the active history. A
pre-connection `captures.sqlite` is offered as a previous capture, not
automatically attributed to a gateway.

The capture table includes source and destination addresses, service, value,
DPT, and ETS group name. A value is decoded only when a supported, unambiguous
DPT is known. Otherwise, group-write and response payloads appear as `0x…`:
these are the group-value bytes, not the entire raw cEMI frame. A read request
has no value payload. Missing or unsupported DPTs are never guessed from byte
length; multiple declarations are marked as ambiguous. Select a row for raw
cEMI and complete DPT details.

| Action | GUI | TUI |
| --- | --- | --- |
| Connect or disconnect | Connection toolbar | `c` |
| Connection settings | Settings… | `s` |
| REST listener settings | REST status in the bottom bar | `a` (confirmed loopback toggle) |
| Filter loaded captures | Filter field | `/` |
| Read / preview a typed write | Read… / Write… | `r` / `w` |
| Import ETS group addresses | Import ETS…; `Cmd-I` on macOS | `i` |
| Export retained history | Export… | `e` |
| Reload / load older history | Reload history / Older history | `h` / `PgUp` |
| Toggle capture colors | Color checkbox; View → Color on macOS | `F8` |

The TUI also offers `d` for gateway discovery, `j`/`k` for scrolling, and `q`
to quit. The macOS app has native application, File, Edit, View, Operation,
Window, and Help menus. Typed writes require a preview followed by a separate
send action in both interfaces.

On attach, the interactive views load the latest 1,000 rows and can page
backward into older retained history. They keep up to 5,000 rows in memory;
the text filter narrows that window, not the entire database. CSV export covers
the complete retained history. Expert raw sending, backup, and durable audit
inspection remain CLI commands; the [capability registry](src/capabilities.rs)
records these explicit differences.

### Import ETS group addresses

First select the target capture by saving its connection settings or opening
a saved capture. Choose **Import ETS…** in the GUI toolbar or **File → Import
ETS Group Addresses…** on macOS (`Cmd-I`). Pick a CSV or XML export, select
the format and CSV encoding, then choose **Preview import**. The preview shows
the target capture, address/DPT counts, and sample entries. **Replace ETS
catalogue** is a separate confirmation; canceling changes nothing.

In the TUI, press `i`, enter the export path, choose `c` for CSV or `x` for XML,
and choose `u` for UTF-8 or `l` for legacy Latin-1 CSV. Review the preview, then
press `y` to confirm or `Esc` to cancel. XML must be UTF-8.

Disconnect the selected capture session before confirming. The GUI's
**Disconnect session** action or `c` in the TUI preview stops only that session;
the daemon and REST listener remain running. A successful import creates a new
metadata revision without changing raw history. Loaded telegrams immediately
gain the new names, DPT declarations, and decodable values while retaining the
current filter and selection.

Standard four-column ETS CSV contains names and addresses, **but no DPTs**.
Importing it improves labels, not typed decoding. GA Export 01 XML and the
extended nine-column KnxMonitor CSV can supply DPT declarations. Full `.knxproj`
projects are not supported. For CLI import, supported typed DPTs, and validation
limits, see the [ETS import guide](docs/ets-import.md).

### Restrained, optional colors

Capture colors are enabled by default; GUI and TUI remember their display
preference independently of the selected database. Human-readable CLI output
uses `--color auto|always|never` or `--no-color`; `auto` is on for terminals and
off for pipes. `NO_COLOR` disables automatic coloring; explicit `--color always`
overrides it. A command-line color override locks the interactive switch for
that process.

Sent telegrams use one restrained accent, router losses are red, and local
subscriber lag is amber. Textual labels remain present without color. JSON
lines, CSV, SQLite, REST, and MCP payloads are never colorized.

## Daemon, CLI, REST, and MCP

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
devknx write-preview --endpoint tunnel://192.0.2.1:3671 --dpt 1.001 1/2/3 true
devknx read --endpoint tunnel://192.0.2.1:3671 1/2/3
devknx write --endpoint tunnel://192.0.2.1:3671 --dpt 1.001 1/2/3 true
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
The explicit `--dpt 1.001` is an example, not a type inferred for the address;
use the actual device's DPT. It may be omitted when ETS supplies an unambiguous,
supported declaration.

The daemon owns one opt-in REST listener, bound to `127.0.0.1:8765` by
default. It can start before any KNX session. `GET`, `POST`, and `DELETE` on
`/v1/sessions` list, connect, and disconnect explicit endpoints. Data and
operations require an endpoint unless a listener default was selected. The GUI
controls the listener in its own REST API dialog, opened from the live status
in the bottom bar.
The TUI's `a` key toggles a loopback listener after confirmation. The CLI
also exposes the bind address, bearer token and remote-write policy. See the
[REST API guide](docs/rest.md) for versioned routes,
SSE resume, authentication and remote-write policy.
The MCP adapter is a separate local stdio process with explicit session list,
connect, and selected-database disconnect tools. It hides typed writes by
default. See the [MCP guide](docs/mcp.md) for tool names, structured results,
bounded search and exact-address write opt-in.

`history --after ... --limit ... --filter ...` filters one bounded CLI page.

### Capture and loss diagnostics

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
Passive receive, restart, local IPC, and SQLite backup have been exercised
against a real KNXnet/IP tunnel on macOS ARM64 and a physical multicast router
on Linux x86_64. The observed bus traffic contained group writes, not reads or
responses. No real router loss report has been observed in the qualification
window; parser and integration tests exercise the diagnostic path. See the
[M2 evidence and limits](https://github.com/metaneutrons/devknx/issues/5).

## Storage and operation safety

`connect` and `monitor` create a versioned SQLite capture store for the selected
endpoint. Selecting a connection in the GUI or TUI also prepares its store;
preparing storage does not connect to KNX. By default, the private per-user path
in the operating system's application-data directory encodes the canonical
mode, IP address, and port; `--database` overrides it. Each committed telegram
receives a monotonic ID; the default retention limit is
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

ETS imports are bounded, validated, and all-or-nothing. Previous metadata
revisions remain intact, and all declared DPTs are retained rather than selecting
the first. Disconnect the selected session before an import: its single-writer
lease also protects metadata updates. See the [ETS import and safety
rules](docs/ets-import.md).

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
`dist/devknx.app`. Published release bundles are signed and notarized;
local unsigned builds are not equivalent to those downloads. The app requires
macOS 12 or newer and provides a native **Check for
Updates…** menu item. Sparkle uses a separately signed update archive and the
feed at [`devknx.metaneutrons.cc/appcast.xml`](https://devknx.metaneutrons.cc/appcast.xml);
the CLI package remains a separate Homebrew formula. The
[update and signing procedure](docs/sparkle-updates.md)
documents the release boundary.

`resources/icon-master.png` is the source for the macOS `.icns`, Windows
executable `.ico`, and Linux icon sizes. Embedded font licences are
listed in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

## Platforms and release artifacts

The release provides the following platform-specific artifacts. The non-publishing
[release-candidate build](docs/release-preparation.md) produces temporary CI
artifacts for packaging tests; it is not a release.

| Platform | Architectures | Deliverables |
| --- | --- | --- |
| macOS | ARM64 only | CLI archive, notarized `.app.zip`, Homebrew `devknx` CLI formula and separate `devknx-app` cask |
| Linux | x86_64 and ARM64 | GNU and musl CLI archives, Debian packages, Homebrew formula, AUR packages |
| Windows | x86_64 and ARM64 | CLI archives with the icon embedded in each executable |

Release artifacts carry SHA-256 checksums, keyless cosign bundles, GitHub
provenance, and SPDX SBOMs. Package definitions are also signed and attested.
Packages are published through GitHub Releases, the
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
links the native GUI live/reconnect CI run and the TUI 5,000-row test. Local
macOS visual checks cover the connection workflow, capture layout, and ETS
import; automated tests do not establish full cross-platform visual qualification.

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
