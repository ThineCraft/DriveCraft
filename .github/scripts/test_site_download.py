"""Tests for site_download.py. Run: python -m unittest discover -s .github/scripts"""

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

import site_download as sd

PAGE = """<html><body>
<p>before</p>
<!-- latest-installer:start -->
old download block
<!-- latest-installer:end -->
<p>after</p>
</body></html>
"""


class PlaceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.docs = root / "docs"
        self.docs.mkdir()
        (self.docs / "index.html").write_text(PAGE, encoding="utf-8")
        self.releases = root / "releases"
        self.releases.mkdir()

    def tearDown(self):
        self.tmp.cleanup()

    def installer(self, version, content=b"installer bytes"):
        f = self.releases / f"DriveCraft_{version}_x64-setup.exe"
        f.write_bytes(content)
        return f

    def test_places_the_installer_its_checksum_and_the_page(self):
        f = self.installer("0.1.1")
        latest = sd.place(f, "0.1.1", self.docs)
        sha = hashlib.sha256(b"installer bytes").hexdigest()
        self.assertEqual((self.docs / "download" / f.name).read_bytes(), b"installer bytes")
        self.assertEqual((self.docs / "download" / f"{f.name}.sha256").read_text(), f"{sha}  {f.name}\n")
        self.assertEqual(json.loads((self.docs / "download" / "latest.json").read_text())["sha256"], sha)
        self.assertEqual(latest["file"], f"/download/{f.name}")
        page = (self.docs / "index.html").read_text(encoding="utf-8")
        self.assertIn(f'href="/download/{f.name}"', page)
        self.assertIn("Download DriveCraft 0.1.1", page)
        self.assertIn(sha, page)
        self.assertNotIn("old download block", page)
        self.assertTrue(page.startswith("<html><body>\n<p>before</p>"))
        self.assertIn("<p>after</p>", page)

    def test_a_newer_version_replaces_the_older_one(self):
        sd.place(self.installer("0.1.1"), "0.1.1", self.docs)
        sd.place(self.installer("0.1.2", b"newer"), "0.1.2", self.docs)
        names = sorted(p.name for p in (self.docs / "download").iterdir())
        self.assertEqual(names, ["DriveCraft_0.1.2_x64-setup.exe", "DriveCraft_0.1.2_x64-setup.exe.sha256", "latest.json"])
        page = (self.docs / "index.html").read_text(encoding="utf-8")
        self.assertNotIn("0.1.1", page)
        self.assertEqual(page.count("latest-installer:start"), 1)

    def test_placing_the_same_version_twice_changes_nothing(self):
        sd.place(self.installer("0.1.1"), "0.1.1", self.docs)
        first = (self.docs / "index.html").read_text(encoding="utf-8")
        sd.place(self.installer("0.1.1"), "0.1.1", self.docs)
        self.assertEqual((self.docs / "index.html").read_text(encoding="utf-8"), first)

    def test_refuses_an_installer_of_another_version(self):
        with self.assertRaises(SystemExit):
            sd.place(self.installer("0.1.0"), "0.1.1", self.docs)
        self.assertFalse((self.docs / "download").exists())

    def test_refuses_a_page_without_markers(self):
        (self.docs / "index.html").write_text("<html></html>", encoding="utf-8")
        with self.assertRaises(SystemExit):
            sd.place(self.installer("0.1.1"), "0.1.1", self.docs)


if __name__ == "__main__":
    unittest.main()
