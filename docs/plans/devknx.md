# Initiative plan: devknx (v1)

Epic: pending GitHub publication
Decision state: proposed; product scope discussed with Fabian in September 2026

## Outcome and boundaries

Build a Rust KNX monitor with one persistent capture service and consistent CLI,
TUI, native GUI, REST, and MCP interfaces. ETS group address exports enrich
captured telegrams. Group writes use an explicit, validated datapoint type
(DPT). The macOS application and its icon are product deliverables from the
first implementation milestone.

Baseline: `KnxMonitor` provides Falcon-based KNXnet/IP tunneling, routing and
USB monitoring, ETS CSV/XML metadata, a TUI, a browser UI, and CSV export. It
does not provide a persistent capture daemon, MCP, or DPT-aware sending.
`devserial` demonstrates a single operation model, daemon, SQLite capture,
and CLI/TUI/GUI/REST/MCP adapters. Neither repository is a code template for
KNX behavior.

The first stable release supports KNXnet/IP tunneling, routing, and discovery.
USB is a documented compatibility gap against `KnxMonitor` until a KNX USB
backend is designed and qualified. Importing complete `.knxproj` projects and
guessing a DPT from payload bytes are outside the initial release.

This plan does not authorize publication, modifying `KnxMonitor`, or changing
the existing `metaneutrons.cc` project overview. Those actions occur only at
their acceptance stages.

## Design and decisions

- `devknx` is an original GPL-3.0-only Rust application using published
  `knx-rs-core` and `knx-rs-ip` releases. It is not published to crates.io.
- One local daemon owns each configured KNX connection and the capture store.
  A typed operation model is the only place where device actions are defined.
  CLI, TUI, GUI, REST, and MCP are adapters to that model.
- A capture stores the raw cEMI frame as well as parsed source, destination,
  service, payload, connection, direction, and timestamp. The raw event is not
  rewritten when ETS metadata changes. Enrichment is versioned separately.
- SQLite uses versioned migrations, monotonic event IDs, bounded retention, and
  cursor-based reads. A live event bus serves the interactive surfaces and SSE;
  SQLite remains the durable source for history and replay.
- ETS CSV 3/1 and KNX GA Export 01 XML use streaming or bounded parsers. Group
  addresses are keyed by the 16-bit address, while original notation and
  hierarchy are retained for display. All declared DPTs are preserved.
- One preparation path resolves the DPT, parses the typed value, encodes it,
  builds the APDU/cEMI frame, and returns the exact frame for both preview and
  transmission. A missing or ambiguous DPT blocks a typed write unless the
  caller supplies an explicit compatible DPT. Raw writes require a separately
  named expert operation.
- A group read reports request transmission separately from a matching
  response. A bounded timeout reports "no response" rather than a fabricated
  value. A successful send does not imply that an actuator changed state.
- Local IPC is restricted to the current user. REST is disabled by default;
  non-loopback binding requires authentication. Remote writes require an
  explicit configuration decision and are logged with their origin.
- The app icon has one reviewed master asset. Build tooling derives macOS
  `.icns`, Windows `.ico` embedded in the executable, and Linux icon sizes.
  The GUI is a native desktop application, not the browser UI from
  `KnxMonitor`.

### Release targets

| Platform | GitHub archive targets | Additional deliverables |
| --- | --- | --- |
| macOS ARM64 | `aarch64-apple-darwin` | signed, notarized, stapled `.app` in a `.app.zip`; Homebrew cask |
| Linux x86_64/ARM64 | GNU and musl targets for both architectures | `.deb` for `amd64`/`arm64`; AUR source and binary definitions |
| Windows x86_64/ARM64 | MSVC targets for both architectures | icon embedded in each `.exe` |

The Homebrew formula covers macOS ARM64 and Linux x86_64/ARM64. Native GUI
qualification applies to GNU/glibc Linux; musl archives are headless. macOS
Intel is excluded by the product decision. There is no Windows installer or
Authenticode promise in the initial release.

## Delivery and acceptance

### M1: Local repository, application identity, and executable skeleton

Execution: pending GitHub issue
Dependencies: none

- M1-A1: Repository starts cleanly with the pinned Rust toolchain, lockfile,
  lint/test/coverage and dependency policy, Conventional Commit hooks, and
  documented GPL-3.0-only provenance.
- M1-A2: A launchable macOS ARM64 app shell, Windows executable resource, and
  Linux desktop icon are generated from one reviewed source asset. The CLI
  retains a non-GUI entry point.
- M1-A3: CI compiles and tests on macOS ARM64, Linux x86_64/ARM64, and Windows
  x86_64/ARM64. A native Windows ARM run proves executable startup.

### M2: KNX capture and persistence

Execution: pending GitHub issue
Dependencies: M1

- M2-A1: Tunnel, routing, and discovery use `knx-rs-ip`; connection failures,
  reconnects, and lost messages are visible as structured states or events.
- M2-A2: Read, write, and response telegrams preserve raw frames and are
  classified correctly. Tests include loopback tunnel traffic, router traffic,
  malformed frames, and replay across a daemon restart.
- M2-A3: SQLite migrations, cursor reads, bounded retention, export, and
  recovery from an interrupted write are tested with positive and negative
  fixtures.

### M3: ETS metadata and DPT-safe operations

Execution: pending GitHub issue
Dependencies: M2

- M3-A1: ETS CSV 3/1 and GA Export 01 XML fixtures import names, descriptions,
  hierarchy, and every declared DPT. Invalid addresses, encodings, duplicate
  addresses, oversized files, and ambiguous DPT lists have specified outcomes.
- M3-A2: Read results distinguish a matching response from a timeout. A write
  preview and the transmitted frame are byte-identical for one-bit, one-byte,
  two-byte float, and four-byte DPT fixtures.
- M3-A3: Unknown or conflicting DPTs cannot silently produce a typed write.
  Raw sending is explicit and separately identifiable in the audit record.

### M4: Human interfaces

Execution: pending GitHub issue
Dependencies: M2, M3

- M4-A1: CLI, `ratatui` TUI, and native `egui` GUI expose connection state,
  live/history views, filter, ETS labels, raw frame details, read, prepared
  write, and export through the same operation model.
- M4-A2: A capability registry declares GUI and TUI functions. Tests detect
  any unrecorded difference; every documented control has a source anchor.
- M4-A3: GUI/TUI startup, resize, disconnect/reconnect, scrolling and capture
  under sustained traffic are qualified on their supported platforms.

### M5: Automation interfaces

Execution: pending GitHub issue
Dependencies: M2, M3

- M5-A1: Versioned REST routes, OpenAPI schemas, pagination, health and SSE
  resume from event IDs are tested. Non-loopback binding fails closed without
  the required authentication configuration.
- M5-A2: MCP over stdio offers structured discovery, status, capture search,
  ETS lookup, read, write preview, and write operations. It uses the same
  validation and audit path as CLI and REST.
- M5-A3: Tests prove that write restrictions and DPT validation cannot be
  bypassed through another frontend.

### M6: Publication and migration

Execution: pending GitHub issue
Dependencies: M1 through M5

- M6-A1: A hardened release candidate builds seven GitHub CLI archives, one
  notarized macOS `.app.zip`, and two Debian packages. Every payload has
  checksums, SBOM, signature, attestation and a clean-room smoke test.
- M6-A2: Homebrew formula/cask, AUR source/binary packages, and the shared
  `deb.metaneutrons.cc` archive are published only after channel preflight
  and package installation tests. Published bytes match the qualified assets.
- M6-A3: The first stable release is visible on GitHub and all configured
  channels. The existing project overview is checked for its `devknx` entry.
  Then `KnxMonitor` is marked deprecated with a link and an explicit notice
  that devknx does not yet support KNX-USB.

Initiative completion requires acceptance evidence for every criterion, the
stable release, and the `KnxMonitor` handoff. A merge or successful build alone
does not establish publication.

## Migration, risks and verification cost

The existing `KnxMonitor` remains available while `devknx` is developed. Its
repository metadata changes only after M6-A3. Existing KNX bus captures and
ETS exports are never modified in place. SQLite schema upgrades require a
backup/recovery test; an incompatible schema change needs a versioned migration.

Gateway behavior may limit which bus telegrams a tunnel can observe. Routing
and tunnel capture must be qualified against representative real hardware in
addition to loopback tests. No gateway test result is claimed yet. KNX USB is
not part of initial parity. macOS signing, package publication and APT archive
configuration depend on provider state and are verified immediately before
use. No time or runner-cost estimate has been measured.

## Decision changes

- 2026-09-27: Fabian confirmed that KNX-USB is not a prerequisite for the
  first stable IP release or the later `KnxMonitor` deprecation. The gap must
  be disclosed at that handoff.

GitHub issue links will be added after the repository is published and
tracking setup is authorized.
