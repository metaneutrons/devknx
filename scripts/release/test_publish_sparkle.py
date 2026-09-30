"""Offline positive and counter-probes for the signed update feed builder."""

import importlib.util
import io
from pathlib import Path
import unittest
from unittest import mock
import xml.etree.ElementTree as ET


SPEC = importlib.util.spec_from_file_location(
    "publish_sparkle", Path(__file__).with_name("publish-sparkle.py"))
PUBLISH = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PUBLISH)


class FeedTests(unittest.TestCase):
    def test_public_digest_sends_a_named_user_agent(self):
        payload = b"published archive"

        def open_public(request, timeout):
            self.assertEqual(timeout, 60)
            self.assertEqual(request.get_header("User-agent"),
                             "devknx-release-verifier/1.0")
            return io.BytesIO(payload)

        with mock.patch.object(PUBLISH.urllib.request, "urlopen",
                               side_effect=open_public) as open_url:
            PUBLISH.public_digest("https://example.com/archive.zip", payload)
        open_url.assert_called_once()

    def test_new_feed_and_idempotent_republication(self):
        feed, repeated = PUBLISH.feed_with_item(
            None, "0.1.0", "app.zip", "signature", 123)
        self.assertFalse(repeated)
        root = ET.fromstring(feed)
        item = root.find("channel/item")
        self.assertEqual(item.findtext(f"{{{PUBLISH.SPARKLE}}}version"), "0.1.0")
        self.assertEqual(item.findtext(f"{{{PUBLISH.SPARKLE}}}minimumSystemVersion"),
                         "12.0.0")
        self.assertEqual(item.findtext(f"{{{PUBLISH.SPARKLE}}}hardwareRequirements"),
                         "arm64")
        self.assertEqual(item.find("enclosure").get("url"),
                         f"https://{PUBLISH.HOST}/app.zip")
        unchanged, repeated = PUBLISH.feed_with_item(
            feed, "0.1.0", "app.zip", "signature", 123)
        self.assertTrue(repeated)
        self.assertEqual(unchanged, feed)

    def test_newer_release_preserves_history_and_rejects_rollback(self):
        old, _ = PUBLISH.feed_with_item(None, "0.1.0", "old.zip", "old", 11)
        new, _ = PUBLISH.feed_with_item(old, "0.1.1", "new.zip", "new", 12)
        self.assertEqual(len(ET.fromstring(new).findall("channel/item")), 2)
        with self.assertRaisesRegex(ValueError, "older update"):
            PUBLISH.feed_with_item(new, "0.0.9", "rollback.zip", "x", 13)

    def test_conflicting_same_version_and_foreign_feed_fail_closed(self):
        feed, _ = PUBLISH.feed_with_item(None, "0.1.0", "app.zip", "original", 123)
        with self.assertRaisesRegex(ValueError, "conflicts"):
            PUBLISH.feed_with_item(feed, "0.1.0", "app.zip", "changed", 123)
        with self.assertRaisesRegex(ValueError, "another host"):
            PUBLISH.feed_with_item(
                feed.replace(PUBLISH.HOST.encode(), b"other.example"),
                "0.1.1", "next.zip", "x", 124)
        with self.assertRaisesRegex(ValueError, "DTD"):
            PUBLISH.feed_with_item(b"<!DOCTYPE rss>" + feed,
                                   "0.1.1", "next.zip", "x", 124)


if __name__ == "__main__":
    unittest.main()
