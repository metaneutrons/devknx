"""Local positive and failure probes for channel definition generation."""

from __future__ import annotations

import hashlib
import pathlib
import tempfile
import tomllib
import unittest

import package_metadata


class PackageMetadataTests(unittest.TestCase):
    def test_exact_sources_checksums_and_cask_name(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            tag = "v0.2.0"
            for target in package_metadata.TARGETS:
                name = f"devknx-{tag}-{target}.tar.gz"
                (root / name).write_bytes(target.encode())
            (root / f"devknx-{tag}-aarch64-apple-darwin.app.zip").write_bytes(b"app")
            (root / f"devknx-{tag}-source.tar.gz").write_bytes(b"source")
            output = root / "definitions"
            paths = package_metadata.generate("0.2.0", tag, root, output)
            self.assertEqual(len(paths), 4)
            self.assertEqual({path.name for path in paths}, {
                "devknx.rb", "devknx-app.rb", "devknx-bin.PKGBUILD", "devknx.PKGBUILD"
            })
            formula = (output / "devknx.rb").read_text()
            self.assertIn(
                hashlib.sha256(b"aarch64-apple-darwin").hexdigest(), formula
            )
            self.assertIn("depends_on arch: :arm64", formula)
            self.assertIn('depends_on "wayland"', formula)
            self.assertIn('depends_on "mesa"', formula)
            for library in ("libxcursor", "libxi"):
                self.assertIn(f'depends_on "{library}"', formula)
            self.assertIn(
                "%w[libx11 libxcb libxcursor libxi libxkbcommon mesa wayland]", formula
            )
            self.assertIn('formula_opt_lib(name)', formula)
            self.assertIn('LD_LIBRARY_PATH:', formula)
            self.assertIn('shell_output("#{bin}/devknx --version")', formula)
            self.assertNotIn("x86_64-apple-darwin", formula)
            cask = (output / "devknx-app.rb").read_text()
            self.assertIn('cask "devknx-app"', cask)
            self.assertIn(
                "  auto_updates true\n  depends_on arch: :arm64\n"
                "  depends_on macos: :monterey\n\n  app \"devknx.app\"",
                cask,
            )
            self.assertIn("devknx-v0.2.0-x86_64-unknown-linux-gnu",
                          (output / "devknx-bin.PKGBUILD").read_text())
            self.assertIn('_srcdir="devknx-v0.2.0"', (output / "devknx.PKGBUILD").read_text())
            self.assertIn("devknx-v0.2.0-source.tar.gz", (output / "devknx.PKGBUILD").read_text())
            for recipe in ("devknx.PKGBUILD", "devknx-bin.PKGBUILD"):
                content = (output / recipe).read_text()
                for library in ("libxcursor", "libxi", "libxkbcommon-x11", "mesa"):
                    self.assertIn(f"'{library}'", content)
            with self.assertRaisesRegex(ValueError, "refusing to overwrite"):
                package_metadata.generate("0.2.0", tag, root, output)

    def test_invalid_or_missing_inputs(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            with self.assertRaisesRegex(ValueError, "do not agree"):
                package_metadata.generate("0.2.0", "v0.3.0", root, root / "out")
            with self.assertRaisesRegex(ValueError, "missing qualified payload"):
                package_metadata.generate("0.2.0", "v0.2.0", root, root / "out")

    def test_debian_declares_dynamically_loaded_gui_libraries(self) -> None:
        manifest = pathlib.Path(__file__).resolve().parents[2] / "Cargo.toml"
        metadata = tomllib.loads(manifest.read_text())["package"]["metadata"]["deb"]
        dependencies = {item.strip() for item in metadata["depends"].split(",")}
        self.assertIn("$auto", dependencies)
        self.assertTrue({
            "libx11-6", "libx11-xcb1", "libxcursor1", "libxi6", "libxcb1",
            "libxkbcommon0", "libxkbcommon-x11-0", "libwayland-client0",
            "libwayland-cursor0", "libwayland-egl1", "libegl1", "libegl-mesa0",
            "libgl1", "libglx-mesa0", "libgl1-mesa-dri",
        }.issubset(dependencies))
        self.assertNotIn("<", metadata["extended-description"])


if __name__ == "__main__":
    unittest.main()
