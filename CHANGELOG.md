# Changelog

## [0.1.1](https://github.com/metaneutrons/devknx/compare/v0.1.0...v0.1.1) (2026-09-30)


### Bug Fixes

* preserve package licences in Debian qualification containers ([#54](https://github.com/metaneutrons/devknx/issues/54)) ([d0a0c13](https://github.com/metaneutrons/devknx/commit/d0a0c1308d6258a1e7cbfe7b799aa72458996a7a))

## 0.1.0 (2026-09-30)


### Features

* add interactive KNX monitors and native macOS menu ([#25](https://github.com/metaneutrons/devknx/issues/25)) ([0940672](https://github.com/metaneutrons/devknx/commit/09406726f2fe212f97d81a4279667dbd5f6cdeb0))
* add raw KNXnet/IP capture stream ([#12](https://github.com/metaneutrons/devknx/issues/12)) ([6859b90](https://github.com/metaneutrons/devknx/commit/6859b90079dd0cb430c55b3bd70493e914966c97))
* **app:** add daemon-managed KNX sessions ([4ca3fbc](https://github.com/metaneutrons/devknx/commit/4ca3fbc7da8d9e659049a12f164633da3632ea09))
* **capture:** add independent foreground capture service ([#16](https://github.com/metaneutrons/devknx/issues/16)) ([3e5e21d](https://github.com/metaneutrons/devknx/commit/3e5e21d9b5b05e252f15ae21f8c048a84fbca101))
* **capture:** expose secure local status and live IPC ([#19](https://github.com/metaneutrons/devknx/issues/19)) ([9568577](https://github.com/metaneutrons/devknx/commit/9568577ae22a513649d2d66d55265ea163eeba7f))
* **capture:** supervise KNXnet/IP reconnects and live events ([#15](https://github.com/metaneutrons/devknx/issues/15)) ([773b9b3](https://github.com/metaneutrons/devknx/commit/773b9b319af0ffcfa1046e88e871701cb7ec4fd8))
* **display:** add semantic capture colors ([0c60252](https://github.com/metaneutrons/devknx/commit/0c60252a2f90936866b97a596010b6ff6a50a0dc))
* import ETS catalogs from GUI and TUI ([#52](https://github.com/metaneutrons/devknx/issues/52)) ([e66840b](https://github.com/metaneutrons/devknx/commit/e66840b74e2e1063cad9f87466482705098eb3c6))
* import ETS metadata and validate group operations ([b851900](https://github.com/metaneutrons/devknx/commit/b8519002ba3f471e64ade3c6e119254752d49f45))
* **macos:** add signed Sparkle app updates ([7ea9d4c](https://github.com/metaneutrons/devknx/commit/7ea9d4c67f2d578c702224a43c59cb70facf9a62))
* manage KNX sessions through REST and MCP ([#50](https://github.com/metaneutrons/devknx/issues/50)) ([eb7fc23](https://github.com/metaneutrons/devknx/commit/eb7fc234c8eba9516fb4b7c965d47d5b18940f58))
* **mcp:** expose structured KNX tools over stdio ([#30](https://github.com/metaneutrons/devknx/issues/30)) ([6795e7f](https://github.com/metaneutrons/devknx/commit/6795e7f282290cfa58459582074e5e676f9b68bf))
* persist KNX captures in SQLite ([#13](https://github.com/metaneutrons/devknx/issues/13)) ([fe40792](https://github.com/metaneutrons/devknx/commit/fe407925e4f321f9a03d0dfffcb2f13b5b7fdb89))
* **rest:** add versioned automation API with fail-closed access ([#29](https://github.com/metaneutrons/devknx/issues/29)) ([f259c50](https://github.com/metaneutrons/devknx/commit/f259c50c4a7f4c96b183e954727fa54880fc148e))
* **storage:** add consistent non-overwriting backups ([#18](https://github.com/metaneutrons/devknx/issues/18)) ([0738736](https://github.com/metaneutrons/devknx/commit/073873692b7e9ce42ec5ea50896049e7dc987782))


### Bug Fixes

* align capture rows and show undecoded payloads ([#51](https://github.com/metaneutrons/devknx/issues/51)) ([f9bc1b1](https://github.com/metaneutrons/devknx/commit/f9bc1b134db7bb48e260dc54e36b538230fc6c1c))
* **automation:** harden REST and MCP contracts ([#46](https://github.com/metaneutrons/devknx/issues/46)) ([8ee3456](https://github.com/metaneutrons/devknx/commit/8ee34564bd6d754c2b2fdf6761bd45f72c5e29a5)), closes [#45](https://github.com/metaneutrons/devknx/issues/45)
* **gui:** add native macOS color menu toggle ([#44](https://github.com/metaneutrons/devknx/issues/44)) ([74b03c3](https://github.com/metaneutrons/devknx/commit/74b03c3052e3fce32508769c6b2d941fda7835dd))
* **gui:** separate REST API controls from connection settings ([a836745](https://github.com/metaneutrons/devknx/commit/a836745fc4672e6de22e22a618c1f9a5b0c53044))
* identify Sparkle public readback requests ([7615e56](https://github.com/metaneutrons/devknx/commit/7615e569bb67e8442482442e2321368ea6323622))
* persist router-reported routing losses ([4915422](https://github.com/metaneutrons/devknx/commit/4915422362754408635dd63167a270b42f4dc74a))
* **release:** correct Homebrew cask stanza order ([75cf065](https://github.com/metaneutrons/devknx/commit/75cf065495c15d67fc4367a30a45b6c4195fe2e0))
* support long Unix IPC database paths ([c333087](https://github.com/metaneutrons/devknx/commit/c333087b968864d8a5349cd5d04bda76e79e8cda))
