#!/usr/bin/env python3
"""Sign a notarized app ZIP and publish it to a guarded R2 Sparkle feed.

The GitHub release must already be staged. The caller promotes it to Latest
only after this publisher and the other package channels have succeeded.
"""

import argparse
import base64
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
import zipfile

PRODUCT = "devknx"
BUCKET = "devknx-updates"
HOST = "devknx.metaneutrons.cc"
ACCOUNT = "9122b44fa4c05b23985d6a0b779caa01"
APP = "devknx.app"
MIN_SYSTEM = "12.0.0"
SPARKLE = "http://www.andymatuschak.org/xml-namespaces/sparkle"
ET.register_namespace("sparkle", SPARKLE)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def read_app_info(archive):
    with zipfile.ZipFile(archive) as zipped:
        info = plistlib.loads(zipped.read(f"{APP}/Contents/Info.plist"))
        require(f"{APP}/Contents/Frameworks/Sparkle.framework/Versions/B/Sparkle"
                in zipped.namelist(), "the app ZIP lacks Sparkle.framework")
    return info


def signature(archive, signer, private_text, public_text):
    from cryptography.exceptions import InvalidSignature
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric.ed25519 import (
        Ed25519PrivateKey, Ed25519PublicKey)

    seed = base64.b64decode(private_text, validate=True)
    require(len(seed) == 32, "Sparkle key must use the 32-byte seed format")
    private = Ed25519PrivateKey.from_private_bytes(seed)
    public = private.public_key().public_bytes(
        encoding=serialization.Encoding.Raw, format=serialization.PublicFormat.Raw)
    require(base64.b64encode(public).decode() == public_text,
            "release key does not match the app's public key")
    signed = subprocess.run(
        [str(signer), "-p", "--ed-key-file", "-", str(archive)],
        input=private_text.encode(), capture_output=True, timeout=120, check=False)
    # Sparkle may echo malformed key material on error. Do not print its output.
    require(signed.returncode == 0, "Sparkle signer rejected the release key")
    value = signed.stdout.decode("ascii").strip()
    signature_bytes = base64.b64decode(value, validate=True)
    require(len(signature_bytes) == 64, "Sparkle signer returned an invalid signature")
    payload = archive.read_bytes()
    require(0 < len(payload) <= 512 * 1024 * 1024,
            "app archive exceeds the release size limit")
    Ed25519PublicKey.from_public_bytes(public).verify(signature_bytes, payload)
    try:
        Ed25519PublicKey.from_public_bytes(public).verify(
            signature_bytes, bytes([payload[0] ^ 1]) + payload[1:])
    except InvalidSignature:
        pass
    else:
        raise ValueError("a corrupted archive passed signature verification")
    return value, payload


def current_object(client, key, limit):
    from botocore.exceptions import ClientError

    try:
        response = client.get_object(Bucket=BUCKET, Key=key)
    except ClientError as error:
        if error.response.get("Error", {}).get("Code") in ("NoSuchKey", "404", "NotFound"):
            return None
        raise
    body = response["Body"]
    try:
        data = body.read(limit + 1)
    finally:
        body.close()
    require(len(data) <= limit, f"R2 object {key} exceeds the size limit")
    return data, response["ETag"]


def feed_with_item(previous, version, archive_name, signature_text, length):
    if previous is None:
        root = ET.Element("rss", {"version": "2.0"})
        channel = ET.SubElement(root, "channel")
        ET.SubElement(channel, "title").text = f"{PRODUCT} updates"
        ET.SubElement(channel, "link").text = f"https://{HOST}/appcast.xml"
        ET.SubElement(channel, "description").text = f"Signed {PRODUCT} macOS releases"
    else:
        require(len(previous) <= 1_000_000, "existing appcast is too large")
        require(b"<!DOCTYPE" not in previous.upper(), "appcast DTD is forbidden")
        root = ET.fromstring(previous)
        require(root.tag == "rss", "existing appcast is not RSS")
        channels = root.findall("channel")
        require(len(channels) == 1, "existing appcast has an invalid channel count")
        channel = channels[0]
        require(channel.findtext("link") == f"https://{HOST}/appcast.xml",
                "existing appcast belongs to another host")
    existing = [item for item in channel.findall("item")
                if item.findtext(f"{{{SPARKLE}}}version") == version]
    if existing:
        require(len(existing) == 1, "duplicate version in existing appcast")
        enclosure = existing[0].find("enclosure")
        require(enclosure is not None and
                enclosure.get("url") == f"https://{HOST}/{archive_name}" and
                enclosure.get(f"{{{SPARKLE}}}edSignature") == signature_text and
                enclosure.get("length") == str(length),
                "published version conflicts with current release")
        return previous, True
    for old_item in channel.findall("item"):
        old_version = old_item.findtext(f"{{{SPARKLE}}}version")
        require(old_version is not None and re.fullmatch(r"\d+\.\d+\.\d+", old_version),
                "existing appcast has an invalid version")
        require(tuple(map(int, version.split("."))) >
                tuple(map(int, old_version.split("."))),
                "refusing to publish an older update")
    item = ET.Element("item")
    ET.SubElement(item, "title").text = f"Version {version}"
    ET.SubElement(item, "link").text = (
        f"https://github.com/metaneutrons/devknx/releases/tag/v{version}")
    ET.SubElement(item, f"{{{SPARKLE}}}version").text = version
    ET.SubElement(item, f"{{{SPARKLE}}}shortVersionString").text = version
    ET.SubElement(item, f"{{{SPARKLE}}}minimumSystemVersion").text = MIN_SYSTEM
    ET.SubElement(item, f"{{{SPARKLE}}}hardwareRequirements").text = "arm64"
    ET.SubElement(item, "pubDate").text = datetime.now(timezone.utc).strftime(
        "%a, %d %b %Y %H:%M:%S +0000")
    ET.SubElement(item, "enclosure", {
        "url": f"https://{HOST}/{archive_name}",
        f"{{{SPARKLE}}}edSignature": signature_text,
        "length": str(length), "type": "application/octet-stream",
    })
    channel.insert(3, item)
    return ET.tostring(root, encoding="utf-8", xml_declaration=True), False


def public_digest(url, expected):
    wanted = hashlib.sha256(expected).digest()
    for attempt in range(12):
        try:
            with urllib.request.urlopen(url, timeout=60) as response:
                digest = hashlib.sha256()
                while chunk := response.read(1024 * 1024):
                    digest.update(chunk)
            if digest.digest() == wanted:
                return
        except (urllib.error.URLError, TimeoutError):
            pass
        if attempt < 11:
            time.sleep(5)
    raise ValueError("public R2 bytes differ from the staged release")


def main():
    import boto3
    from botocore.config import Config

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--signer", type=Path, required=True)
    parser.add_argument("--version", required=True)
    args = parser.parse_args()
    require(re.fullmatch(r"\d+\.\d+\.\d+", args.version), "stable SemVer required")
    require(args.archive.name == f"{PRODUCT}-v{args.version}-aarch64-apple-darwin.app.zip",
            "unexpected app archive name")
    info = read_app_info(args.archive)
    require(info["CFBundleIdentifier"] == "cc.metaneutrons.devknx", "wrong app identity")
    require(info["CFBundleVersion"] == args.version and
            info["CFBundleShortVersionString"] == args.version, "app version mismatch")
    require(info["SUFeedURL"] == f"https://{HOST}/appcast.xml", "wrong feed URL")
    private = os.environ["SPARKLE_ED_PRIVATE_KEY"].strip()
    signed, archive = signature(args.archive, args.signer, private, info["SUPublicEDKey"])
    client = boto3.client(
        "s3", endpoint_url=f"https://{ACCOUNT}.r2.cloudflarestorage.com",
        region_name="auto", aws_access_key_id=os.environ["R2_ACCESS_KEY_ID"],
        aws_secret_access_key=os.environ["R2_SECRET_ACCESS_KEY"],
        config=Config(signature_version="s3v4"))
    archive_name = args.archive.name
    old = current_object(client, "appcast.xml", 1_000_000)
    feed, unchanged = feed_with_item(
        old[0] if old else None, args.version, archive_name, signed, len(archive))
    remote_archive = current_object(client, archive_name, 512 * 1024 * 1024)
    if remote_archive is None:
        client.put_object(Bucket=BUCKET, Key=archive_name, Body=archive,
                          ContentType="application/zip",
                          CacheControl="public, max-age=31536000, immutable",
                          IfNoneMatch="*")
    else:
        require(remote_archive[0] == archive, "R2 archive conflicts with staged bytes")
    public_digest(f"https://{HOST}/{archive_name}", archive)
    if not unchanged:
        condition = {"IfMatch": old[1]} if old else {"IfNoneMatch": "*"}
        client.put_object(Bucket=BUCKET, Key="appcast.xml", Body=feed,
                          ContentType="application/xml; charset=utf-8",
                          CacheControl="no-cache", **condition)
    stored = current_object(client, "appcast.xml", 1_000_000)
    require(stored is not None and stored[0] == feed, "R2 appcast read-back failed")
    public_digest(f"https://{HOST}/appcast.xml", feed)
    print(json.dumps({"product": PRODUCT, "version": args.version,
                      "archive_sha256": hashlib.sha256(archive).hexdigest(),
                      "feed_sha256": hashlib.sha256(feed).hexdigest(),
                      "idempotent": unchanged}, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Neither the signer diagnostics nor environment variables are safe to log.
        print(f"Sparkle publication failed ({type(error).__name__}); no secret output", file=sys.stderr)
        sys.exit(1)
