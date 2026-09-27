import pathlib
import tarfile
import tempfile
import unittest
import zipfile

from package_cli import package, valid_executable


class PackageCliTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        (self.root / "Cargo.toml").write_text('[package]\nversion = "0.1.0"\n')
        (self.root / "LICENSE").write_text("GPL test notice")
        (self.root / "THIRD-PARTY-NOTICES.md").write_text("Font test notice")
        (self.root / "licenses").mkdir()
        (self.root / "licenses" / "OFL.txt").write_text("OFL test notice")
        self.binary = self.root / "devknx"
        self.binary.write_bytes(b"\x7fELF\x02\x01" + b"\0" * 12 + (62).to_bytes(2, "little"))

    def test_tar_is_reproducible_and_complete(self):
        first = package(self.root, "v0.1.0", "x86_64-unknown-linux-gnu", self.binary, self.root / "a", 1700000000)
        second = package(self.root, "v0.1.0", "x86_64-unknown-linux-gnu", self.binary, self.root / "b", 1700000000)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        with tarfile.open(first) as archive:
            names = archive.getnames()
        self.assertEqual(len(names), 4)
        self.assertTrue(any(name.endswith("/devknx") for name in names))
        self.assertTrue(any(name.endswith("/licenses/OFL.txt") for name in names))

    def test_zip_is_reproducible_and_complete(self):
        header = bytearray(72)
        header[:2] = b"MZ"
        header[60:64] = (64).to_bytes(4, "little")
        header[64:68] = b"PE\0\0"
        header[68:70] = (0xAA64).to_bytes(2, "little")
        self.binary.write_bytes(header)
        first = package(self.root, "v0.1.0", "aarch64-pc-windows-msvc", self.binary, self.root / "a", 1700000000)
        second = package(self.root, "v0.1.0", "aarch64-pc-windows-msvc", self.binary, self.root / "b", 1700000000)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        with zipfile.ZipFile(first) as archive:
            names = archive.namelist()
        self.assertTrue(any(name.endswith("/devknx.exe") for name in names))
        self.assertTrue(any(name.endswith("/THIRD-PARTY-NOTICES.md") for name in names))

    def test_wrong_architecture_fails(self):
        with self.assertRaisesRegex(ValueError, "format does not match"):
            package(self.root, "v0.1.0", "aarch64-unknown-linux-gnu", self.binary, self.root / "a", 1700000000)

    def test_missing_notice_fails_without_archive(self):
        (self.root / "THIRD-PARTY-NOTICES.md").unlink()
        with self.assertRaisesRegex(ValueError, "missing licence notice"):
            package(self.root, "v0.1.0", "x86_64-unknown-linux-gnu", self.binary, self.root / "a", 1700000000)
        self.assertFalse((self.root / "a").exists())

    def test_invalid_tag_fails(self):
        with self.assertRaisesRegex(ValueError, "invalid release tag"):
            package(self.root, "v0.1.0/../../bad", "x86_64-unknown-linux-gnu", self.binary, self.root / "a", 1700000000)

    def test_overwrite_fails(self):
        package(self.root, "v0.1.0", "x86_64-unknown-linux-gnu", self.binary, self.root / "a", 1700000000)
        with self.assertRaisesRegex(ValueError, "refusing to overwrite"):
            package(self.root, "v0.1.0", "x86_64-unknown-linux-gnu", self.binary, self.root / "a", 1700000000)

    def test_pe_machine_must_match(self):
        header = bytearray(72)
        header[:2] = b"MZ"
        header[60:64] = (64).to_bytes(4, "little")
        header[64:68] = b"PE\0\0"
        header[68:70] = (0x8664).to_bytes(2, "little")
        self.assertTrue(valid_executable(header, "x86_64-pc-windows-msvc"))
        self.assertFalse(valid_executable(header, "aarch64-pc-windows-msvc"))


if __name__ == "__main__":
    unittest.main()
