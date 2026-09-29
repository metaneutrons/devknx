# Third-party notices

The GUI uses `eframe`'s default fonts. `epaint_default_fonts` 0.36.2 embeds
the following four typefaces in a GUI binary. Their licence notices accompany
this source tree and must accompany every distributed GUI binary.

| Typeface | Licence | Notice |
| --- | --- | --- |
| Noto Emoji | OFL-1.1 | [OFL-1.1.txt](licenses/OFL-1.1.txt) |
| Ubuntu Light | Ubuntu Font Licence 1.0 | [UFL-1.0.txt](licenses/UFL-1.0.txt) |
| Hack | MIT, with DejaVu and Bitstream Vera provenance | [Hack.txt](licenses/Hack.txt) |
| emoji-icon-font | MIT | [emoji-icon-font-MIT.txt](licenses/emoji-icon-font-MIT.txt) |

The four notice texts were copied verbatim from the
`epaint_default_fonts` 0.36.2 source package. Ordinary Rust dependencies are
recorded in `Cargo.lock` and checked against `deny.toml`.

The macOS application bundle includes Sparkle 2.10.0. Its complete licence,
including notices for bundled components, is in
`devknx.app/Contents/Resources/Sparkle-LICENSE`. The standalone CLI
archives do not include Sparkle.
