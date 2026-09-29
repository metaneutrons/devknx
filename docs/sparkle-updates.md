# macOS app updates

The notarized ARM64 `devknx.app` supports macOS 12 or newer and embeds Sparkle
2.10.0. The standalone CLI archive and Homebrew formula do not contain
Sparkle. The feed is `https://devknx.metaneutrons.cc/appcast.xml`, served by
the `devknx-updates` Cloudflare R2 bucket.

The candidate job downloads a SHA-256-pinned Sparkle distribution and embeds
its framework. The release job signs the nested helpers and outer app,
notarizes and staples it. The `publish-sparkle` job takes that exact `.app.zip`
after GitHub staging and the Homebrew and AUR channels, signs it with the
dedicated Sparkle Ed25519 key, verifies the signature against the app's
`SUPublicEDKey`, and rejects a corrupted-byte probe. It writes the ZIP at an
immutable R2 key, reads back authenticated and public bytes, then appends to
the feed using a conditional write. Identical retries are idempotent;
conflicting or older releases fail closed. Latest promotion depends on this
job's success.

The protected `release-sparkle` GitHub environment accepts only `v*` tags.
It contains `SPARKLE_ED_PRIVATE_KEY` (the original Sparkle-exported base64
seed) and bucket-scoped `R2_ACCESS_KEY_ID` and `R2_SECRET_ACCESS_KEY`.
Never commit these values. The private Sparkle key is backed up offline and
must remain available for future updates of installed versions. Key rotation
requires a separately qualified transition release.

The first stable Sparkle-enabled release creates the feed. Until then, an
empty bucket returns HTTP 404. Do not manually replace a signed archive or
feed entry to repair a failed release; fix the cause and rerun the immutable
workflow.
