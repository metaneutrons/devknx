# Changelog

## [0.5.0](https://github.com/metaneutrons/devknx/compare/v0.4.1...v0.5.0) (2026-10-15)


### Features

* **daemon:** delete the telegrams above or below a telegram from the window and the terminal ([#88](https://github.com/metaneutrons/devknx/issues/88)) ([eb20001](https://github.com/metaneutrons/devknx/commit/eb20001ff519f1b9e631664e5e876e299d75b50f))


### Bug Fixes

* **cli:** report a failed command as one line, not as the Debug form of its error ([#87](https://github.com/metaneutrons/devknx/issues/87)) ([2c3a0b9](https://github.com/metaneutrons/devknx/commit/2c3a0b9cdd4dab11463d61d32e4afb93c32ab266))

## [0.4.1](https://github.com/metaneutrons/devknx/compare/v0.4.0...v0.4.1) (2026-10-15)


### Bug Fixes

* **daemon:** start another daemon when the one a client started has ended, and wait longer ([#84](https://github.com/metaneutrons/devknx/issues/84)) ([e8d96ae](https://github.com/metaneutrons/devknx/commit/e8d96aebb07406fd73243c4f9ada5a50c703074a))

## [0.4.0](https://github.com/metaneutrons/devknx/compare/v0.3.1...v0.4.0) (2026-10-15)


### Features

* **daemon:** clear the stored capture from the window and the terminal, with a copy first ([#81](https://github.com/metaneutrons/devknx/issues/81)) ([3d84e0a](https://github.com/metaneutrons/devknx/commit/3d84e0a6a9416ffe8b1f165fb2cac245944daedf))
* **gui:** Cmd+F selects the text of the filter field and works off macOS ([#78](https://github.com/metaneutrons/devknx/issues/78)) ([e0f2ba8](https://github.com/metaneutrons/devknx/commit/e0f2ba8561f5fd77c97c4ecad2136dad215550f5))

## [0.3.1](https://github.com/metaneutrons/devknx/compare/v0.3.0...v0.3.1) (2026-10-13)


### Bug Fixes

* **daemon:** do not warn about a daemon of the same build without git ([#75](https://github.com/metaneutrons/devknx/issues/75)) ([b2c64fa](https://github.com/metaneutrons/devknx/commit/b2c64fa52f3f70f1bd02d464889cdd0738c6bfa2))

## [0.3.0](https://github.com/metaneutrons/devknx/compare/v0.2.0...v0.3.0) (2026-10-13)


### Features

* **daemon:** stop by itself when idle ([#72](https://github.com/metaneutrons/devknx/issues/72)) ([65b6fbf](https://github.com/metaneutrons/devknx/commit/65b6fbf931e0a16c3f8165c1d60ccdcac94ac990))
* **daemon:** warn when the daemon is another build ([#70](https://github.com/metaneutrons/devknx/issues/70)) ([40c5384](https://github.com/metaneutrons/devknx/commit/40c5384b28c3b33a64b91eadf86a6cc8483ab99f))
* **gui:** ask about the daemon when quitting ([#73](https://github.com/metaneutrons/devknx/issues/73)) ([aa5cc78](https://github.com/metaneutrons/devknx/commit/aa5cc789c17146bcc81822641966dd4862ff58c5))


### Bug Fixes

* **daemon:** start a daemon when the first ping ends without an answer ([#74](https://github.com/metaneutrons/devknx/issues/74)) ([5fb638a](https://github.com/metaneutrons/devknx/commit/5fb638a5497bb9e688c5d2c69e9fc5243721477b))

## [0.2.0](https://github.com/metaneutrons/devknx/compare/v0.1.0...v0.2.0) (2026-10-12)


### Features

* ship an agent skill and keep it current ([#68](https://github.com/metaneutrons/devknx/issues/68)) ([d805fe9](https://github.com/metaneutrons/devknx/commit/d805fe95e11e4cadd8f526b00bdc1af45548a940))

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
