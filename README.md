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
> **Early development.** There is no stable release yet. The current code discovers KNXnet/IP gateways from a CLI and a native GUI shell. It does **not** yet monitor telegrams, import ETS exports, or send group values. Do not use it to operate a live installation.

## What devknx is building

devknx combines the KNX protocol implementation in the
[`knx-rs` crate family](https://github.com/metaneutrons/knx-rs) with a
persistent local capture service. A single typed operation model will serve
every interface, so a DPT validation rule cannot silently differ between a
desktop button and an API call.

| Capability | Current state |
| --- | --- |
| KNXnet/IP gateway discovery | CLI and native GUI shell available |
| KNXnet/IP tunneling and routing capture | Planned |
| Durable, searchable telegram history | Planned |
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
cargo run --locked -- gui
```

Gateway discovery sends KNXnet/IP multicast on the local network. Network
equipment and host firewall rules can affect the result. The GUI can also be
opened without a gateway; its current purpose is to exercise the native app
shell and discovery view.

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
