"""Local positive and failure probes for channel definition generation."""

from __future__ import annotations

import hashlib
import pathlib
import tempfile
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
            self.assertIn('formula_opt_lib(name)', formula)
            self.assertIn('LD_LIBRARY_PATH:', formula)
            self.assertIn('shell_output("#{bin}/devknx --version")', formula)
            self.assertNotIn("x86_64-apple-darwin", formula)
            cask = (output / "devknx-app.rb").read_text()
            self.assertIn('cask "devknx-app"', cask)
            self.assertIn("depends_on macos: :monterey", cask)
            self.assertIn("depends_on arch: :arm64", cask)
            self.assertIn("auto_updates true", cask)
            self.assertIn("devknx-v0.2.0-x86_64-unknown-linux-gnu",
                          (output / "devknx-bin.PKGBUILD").read_text())
            self.assertIn('_srcdir="devknx-v0.2.0"', (output / "devknx.PKGBUILD").read_text())
            self.assertIn("devknx-v0.2.0-source.tar.gz", (output / "devknx.PKGBUILD").read_text())
            with self.assertRaisesRegex(ValueError, "refusing to overwrite"):
                package_metadata.generate("0.2.0", tag, root, output)

    def test_invalid_or_missing_inputs(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            with self.assertRaisesRegex(ValueError, "do not agree"):
                package_metadata.generate("0.2.0", "v0.3.0", root, root / "out")
            with self.assertRaisesRegex(ValueError, "missing qualified payload"):
                package_metadata.generate("0.2.0", "v0.2.0", root, root / "out")


if __name__ == "__main__":
    unittest.main()
