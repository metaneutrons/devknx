# Initiative plan: devknx (v9)

Epic: [devknx initiative](https://github.com/metaneutrons/devknx/issues/3)
Decision state: product scope agreed with Fabian in September 2026

## Outcome and boundaries

Build a Rust KNX monitor with persistent capture services and consistent CLI,
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

The public repository is the project workspace, not a software release. This
plan does not authorize releasing binaries or packages, modifying `KnxMonitor`,
or changing the existing `metaneutrons.cc` project overview. Those actions
occur only at their acceptance stages.

## Design and decisions

- `devknx` is an original GPL-3.0-only Rust application using published
  `knx-rs-core` and `knx-rs-ip` releases. It is not published to crates.io.
- One per-user local daemon can run without a KNX connection. It manages
  independently connected KNX sessions, each with its own capture service,
  database writer and local operation endpoint. A typed operation model is
  the only place where device actions are defined. CLI, TUI, GUI, REST, and
  MCP are adapters to that model.
- The default capture database is deterministically derived from the canonical
  KNXnet/IP mode, address, and port. `--database` overrides this selection.
  CLI commands without a positional endpoint require either `--endpoint` or
  `--database`; they never infer a write target from the GUI's last-used
  connection. The GUI may remember that connection for its own startup. The
  TUI requires an explicit selector at startup and may switch endpoint-derived
  stores when the user changes its connection settings.
- `daemon` runs the connection-independent owner in the foreground for service
  managers; `daemon --status` and `daemon --stop` never start it. Explicit
  Connect in GUI/TUI and `connect` in CLI start the daemon on demand, then add
  one selected session. Endpoint-selected live CLI operations may start the
  daemon and connect that endpoint, but offline history, preview, status and
  export never connect to KNX. A database-only selector can use an already
  running owner but cannot infer which endpoint to connect. Disconnecting one
  session leaves the daemon and other sessions running. `monitor` is a client
  of the managed session, not a competing SQLite writer.
- REST is disabled by default and is enabled, inspected, or disabled through
  the daemon control plane. One listener can start without a KNX session or
  selected endpoint. HTTP lists sessions and starts or stops explicitly named
  endpoints; each data and operation request selects an endpoint unless a
  listener default was configured. Disconnecting does not stop REST. No
  implicit cross-session aggregation or target switching is allowed. MCP remains an opt-in stdio adapter to a selected capture, but
  exposes session listing, explicit connection, and a database-scoped
  disconnect even before its selected database exists. The pre-connection
  `captures.sqlite` may mix gateways and remains available for offline
  inspection; it is not silently assigned to one endpoint.
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
- Automation clients see raw captures plus separately versioned, current-ETS
  enrichment. Router-reported routing losses have their own REST/MCP cursors,
  never a capture cursor or a bus-wide loss total. REST SSE replays durable
  history and uses the owner's live feed for wake-ups, with bounded fallback
  checks if the feed is unavailable.
- MCP typed writes are absent from the default tool set. Starting a writable
  stdio adapter requires an explicit switch and at least one exact group
  address in its allowlist. The existing DPT preparation and audit paths
  remain mandatory; an allowlist is not a claim of human confirmation.
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

The Homebrew formula `metaneutrons/tap/devknx` installs the online CLI binary
on macOS ARM64 and Linux x86_64/ARM64. A separate macOS ARM64 cask,
`metaneutrons/tap/devknx-app`, installs the signed, notarized `.app` bundle.
Both packages must be independently installable and coexist, following
devserial's packaging split. Native GUI
qualification applies to GNU/glibc Linux; musl archives are headless. macOS
Intel is excluded by the product decision. There is no Windows installer or
Authenticode promise in the initial release.

## Delivery and acceptance

### M1: Local repository, application identity, and executable skeleton

Tracking: [M1 issue](https://github.com/metaneutrons/devknx/issues/4)
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

Tracking: [M2 issue](https://github.com/metaneutrons/devknx/issues/5)
Dependencies: M1

- M2-A1: Tunnel, routing, and discovery use `knx-rs-ip`; connection failures,
  reconnects, and lost messages are visible as structured states or events.
  Router-reported `RoutingLostMessage` (`0x0531`) diagnostics have a distinct
  durable event stream with source, device state, and count. Local subscriber
  lag is reported per stream, while connection interruptions have no inferred
  loss count. None of these is presented as a general KNX bus-loss total.
- M2-A2: Read, write, and response telegrams preserve raw frames and are
  classified correctly. Tests include loopback tunnel traffic, router traffic,
  malformed frames, and replay across a daemon restart.
- M2-A3: SQLite migrations, cursor reads, bounded retention, export, and
  recovery from an interrupted write are tested with positive and negative
  fixtures.

### M3: ETS metadata and DPT-safe operations

Tracking: [M3 issue](https://github.com/metaneutrons/devknx/issues/6)
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

Tracking: [M4 issue](https://github.com/metaneutrons/devknx/issues/7)
Dependencies: M2, M3

- M4-A1: CLI, `ratatui` TUI, and native `egui` GUI expose connection state,
  live/history views, filter, ETS labels, raw frame details, read, prepared
  write, and export through the same operation model.
- M4-A2: A capability registry declares GUI and TUI functions. Tests detect
  any unrecorded difference; every documented control has a source anchor.
- M4-A3: GUI/TUI startup, resize, disconnect/reconnect, scrolling and capture
  under sustained traffic are qualified on their supported platforms.
- M4-A4: The macOS app has a native application menu modeled on devserial:
  About, standard app/edit/window actions, and working KNX-specific File/View
  actions with keyboard shortcuts. Menu actions use the same GUI operations as
  visible controls; source-anchored tests and a macOS app smoke test verify the
  bridge and bundled identity.

### M5: Automation interfaces

Tracking: [M5 issue](https://github.com/metaneutrons/devknx/issues/8)
Dependencies: M2, M3

- M5-A1: Versioned REST routes, OpenAPI schemas, pagination, health and SSE
  resume from event IDs are tested. Non-loopback binding fails closed without
  the required authentication configuration.
- M5-A2: MCP over stdio offers structured discovery, status, capture search,
  ETS lookup, read, write preview, and write operations. It uses the same
  validation and audit path as CLI and REST.
- M5-A3: Tests prove that write restrictions and DPT validation cannot be
  bypassed through another frontend.

### M7: Connection-independent daemon and explicit session lifecycle

Tracking: [M7 issue](https://github.com/metaneutrons/devknx/issues/40)
Dependencies: M2 through M5; prerequisite to M6 publication

- M7-A1: A single current-user daemon starts without contacting a KNX endpoint.
  An explicit Connect adds an endpoint-scoped capture service and database
  writer; Disconnect stops only that service, releases its writer lease, and
  cannot replay queued operations on a later reconnect. At least two
  independent endpoints can be active. A database override cannot be shared
  by different active endpoints or silently merge their histories.
- M7-A2: GUI, TUI, and CLI share one session lifecycle. Endpoint-selected live
  CLI operations can auto-start the daemon and connect the selected endpoint;
  database-only requests never infer an endpoint. Offline commands and daemon
  status do not create a connection. Monitoring detaches without stopping the
  daemon. Current-user control IPC is authenticated by OS permissions and
  rejects duplicate owners and malformed or oversized requests.
- M7-A3: REST has explicit enable, disable, and status controls. It is off by
  default, starts independently of endpoints or active KNX sessions, and
  reports the actual listening address only after successful bind. Authenticated
  HTTP list, status, connect, and disconnect requests manage explicit sessions;
  endpoint-scoped data and operations never guess among sessions. A configured
  endpoint is only a default; disconnect leaves REST available. Existing
  token, remote-write and origin-audit rules remain effective; status never
  discloses a token. The GUI exposes live status in the bottom bar and policy
  and listener controls in a separate REST API dialog. The TUI offers a
  confirmed loopback-only toggle. MCP lists daemon sessions, connects an
  explicit endpoint to its selected capture, and disconnects only the session
  atomically associated with that capture database. It can start before the
  first capture file exists; typed writes remain separately opted in.
- M7-A4: Documentation, source-anchored interface coverage, loopback tests,
  and supported-platform CI qualify two sessions, isolation, lifecycle,
  operation safety, REST and MCP connection controls before and after a KNX
  connection, daemon startup races, and shutdown. Negative cases include
  invalid endpoints, conflicting database bindings and unauthorized remote
  control. No physical KNX write is required for acceptance.

### M8: Automation contract and safety hardening

Tracking: [M8 issue](https://github.com/metaneutrons/devknx/issues/45)
Dependencies: M5; coordinates with M7; prerequisite to M6 publication

- M8-A1: REST OpenAPI describes actual conditional bearer security, a valid
  `WWW-Authenticate` challenge, JSON error shape, route schemas, and bounded
  request/SSE behavior. Positive and counter-probe tests verify each gate.
- M8-A2: Router-reported losses have distinct REST page/SSE and MCP cursor
  access. MCP has unfiltered capture pagination. Both histories retain
  separate monotonic IDs and retention-gap semantics; local subscriber lag is
  not misreported as KNX routing loss.
- M8-A3: GUI, REST and MCP share one versioned additive ETS enrichment model.
  Captures retain exact raw cEMI bytes, while current ETS names, hierarchy,
  DPTs and safely decoded values are shown separately. A matching group-read
  response is enriched without fabricating a response on timeout. Unknown or
  ambiguous DPTs produce no guessed value.
- M8-A4: MCP typed writes are not listed or callable by default. A writable
  adapter requires explicit CLI opt-in and one or more exact allowed group
  addresses, checked again at invocation. Bounded tool/bus call rates and
  accurate risk annotations accompany the existing validation and audit path.
  No physical KNX write is required for acceptance.
- M8-A5: Documentation, focused integration tests, and supported CI checks
  verify the adapter contract and misuse cases. This milestone does not
  publish packages or alter `KnxMonitor`.

### M6: Publication and migration

Tracking: [M6 issue](https://github.com/metaneutrons/devknx/issues/9)
Dependencies: M1 through M5, M7 and M8

- M6-A1: A hardened release candidate builds seven GitHub CLI archives, one
  notarized macOS `.app.zip`, one deterministic source archive for the AUR
  source package, and two Debian packages. Every payload has
  checksums, SBOM, signature, attestation and a clean-room smoke test. Every
  GUI payload also includes the embedded-font licence notices.
- M6-A2: Homebrew CLI formula and macOS app cask, AUR source/binary packages, and the shared
  `deb.metaneutrons.cc` archive are published only after channel preflight
  and package installation tests. Both Homebrew packages install side by side;
  the formula exposes `devknx` on `PATH` and the cask launches `devknx.app`.
  Published bytes match the qualified assets.
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
addition to loopback tests. Measured results belong in the M2 issue; this plan
does not itself claim a gateway result. KNX USB is
not part of initial parity. macOS signing, package publication and APT archive
configuration depend on provider state and are verified immediately before
use. No time or runner-cost estimate has been measured.

## Decision changes

- 2026-09-27 (v1): Fabian confirmed that KNX-USB is not a prerequisite for the
  first stable IP release or the later `KnxMonitor` deprecation. The gap must
  be disclosed at that handoff.
- 2026-09-27 (v2): Fabian confirmed that M2-A1 includes router-reported
  `RoutingLostMessage` diagnostics. Router reports, per-stream local subscriber
  lag, and unquantified connection interruptions remain separate; no category
  is presented as a general KNX bus-loss total.
- 2026-09-27 (v3): Fabian included a native macOS application menu in M4,
  following devserial's system-menu integration without importing its serial
  device controls. He reaffirmed that M6 distributes the macOS ARM64 CLI as
  a Homebrew formula and the app bundle as a separate co-installable cask.
- 2026-09-27 (v4): Release preparation adds one deterministic, attested source
  archive for the AUR source package. It supplements the agreed binary matrix;
  it does not add a platform or authorize publication.
- 2026-09-28 (v5): Endpoint-derived private databases replace one implicit CLI
  profile. The GUI may remember a connection, while CLI write targets require
  an explicit endpoint or database selector.
- 2026-09-28 (v6): Fabian requested devserial-style automatic daemon startup
  for appropriate CLI calls and a connection lifecycle separate from daemon
  lifetime. M7 makes this a prerequisite of the first release. Release-Please
  PRs remain manual; no merge, tag, or publication is authorized here.
- 2026-09-30 (v8): Fabian clarified that REST and MCP must control the KNX
  connection themselves. M7-A3 replaces the earlier active-session requirement
  for REST enablement and the automatic REST stop on KNX disconnect.
- 2026-09-30 (v9): REST can start with no default endpoint, list and control
  explicitly named sessions, and scope every data and operation request to a
  named endpoint. A configured listener endpoint remains an optional default.
