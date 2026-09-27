"""Counter-probes for exact release inventories and checksum reconciliation."""

from __future__ import annotations

import pathlib
import tempfile
import unittest

import release_inventory


class ReleaseInventoryTests(unittest.TestCase):
    def test_prerelease_names_retain_full_tag_but_deb_uses_core(self) -> None:
        names = release_inventory.payloads("v0.2.0-rc.1")
        self.assertEqual(len(names), 11)
        self.assertIn("devknx-v0.2.0-rc.1-source.tar.gz", names)
        self.assertIn("devknx-v0.2.0-rc.1-aarch64-apple-darwin.app.zip", names)
        self.assertIn("devknx_0.2.0-1_amd64.deb", names)

    def test_full_inventory_and_tamper_detection(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            names = release_inventory.expected("v0.2.0", complete=True)
            for name in names - {"SHA256SUMS"}:
                (root / name).write_bytes(name.encode())
            release_inventory.write_sums(root, "v0.2.0")
            release_inventory.verify(root, "v0.2.0")
            one = root / "devknx.rb"
            one.write_bytes(b"changed")
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                release_inventory.verify(root, "v0.2.0")

    def test_missing_extra_symlink_and_invalid_tag(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            with self.assertRaisesRegex(ValueError, "missing="):
                release_inventory.check_set(root, {"required"})
            (root / "extra").write_bytes(b"x")
            with self.assertRaisesRegex(ValueError, "unexpected="):
                release_inventory.check_set(root, set())
            (root / "extra").unlink()
            (root / "link").symlink_to(root / "outside")
            with self.assertRaisesRegex(ValueError, "unsafe"):
                release_inventory.file_names(root)
            with self.assertRaisesRegex(ValueError, "invalid release tag"):
                release_inventory.payloads("0.2.0")


if __name__ == "__main__":
    unittest.main()
