# Release preparation

The release-candidate workflow is a non-publishing qualification step. It runs
on packaging pull requests and can also be dispatched manually. Its token has
read-only repository access. It creates no tag, GitHub Release, package-channel
update, signature, or notarization submission.

Release Please uses the single-crate Rust strategy. A 0.0.0 bootstrap
manifest proposed 0.1.0 as the first version. Release Please synchronizes
`Cargo.toml`, `Cargo.lock`, the manifest, and `CHANGELOG.md`. The proposal
must remain unmerged until the release is separately authorized. The
Release-Please PR is reviewed and merged manually; auto-merge must remain off.
Only its
merge creates the immutable tag and draft GitHub release, then dispatches
the hardened pipeline against that exact tag.

Its short-lived CI artifacts contain seven target-specific CLI archives, a
tagged source archive, an unsigned macOS ARM64 app ZIP, and two Debian packages:

| Target | Candidate payloads |
| --- | --- |
| macOS ARM64 | CLI `.tar.gz`, unsigned `.app.zip` |
| Linux x86_64/ARM64 GNU | CLI `.tar.gz`, `.deb` |
| Linux x86_64/ARM64 musl | headless CLI `.tar.gz` |
| Windows x86_64/ARM64 | CLI `.zip` |
| All platforms | One deterministic tagged source `.tar.gz` for the AUR source package |

The GNU and macOS archives carry the GUI-capable build. The musl builds are
headless. Every CLI archive includes the GPL licence and the embedded-font
licence notices; the app bundles the notices under `Contents/Resources`.
The packaging script rejects a mismatched executable format or architecture,
an invalid tag, missing notices, and an overwrite. It normalizes archive
ordering and timestamps from the commit epoch. The workflow extracts every
archive and runs its binary before retaining it as a CI artifact.

Candidate artifacts are **not** ready for distribution. The separate
`release.yml` workflow is dispatched against an immutable tag only after
release-please merges a version/changelog pull request. It reuses the
candidate builds, signs the ARM64 app with Developer ID, submits it for Apple
notarization, staples it, and tests the distributed ZIP with Gatekeeper. The
pipeline then creates SPDX SBOMs, keyless cosign bundles and GitHub provenance
for each of the eleven payloads; it generates and attests four channel
definitions and checks the exact release inventory before staging anything.
The `SHA256SUMS` file covers every other asset and is itself attested.

The release initially becomes a GitHub prerelease, never `latest`. The
pipeline installs candidate Debian, Homebrew and both AUR packages before
staging. For a stable tag, channel preflight must also pass. It publishes the
Homebrew formula/cask through a checked tap PR, publishes both AUR recipes
through pinned SSH, verifies public bytes and package versions, and only then
promotes the unchanged GitHub release to `latest`. The project holds no APT
signing or object-store key: after promotion it dispatches the central
`apt-archive` workflow and reads the signed APT client path back on amd64
and arm64. A failed APT read-back leaves the previous index in service; it
cannot roll back a GitHub release already promoted.

GitHub has three isolated environments. `release-please` accepts only
`main` and holds the shared Release Please App key; the workflow requests an
installation token limited to `devknx` with only Contents and Pull Requests
write access. The App key itself can mint tokens for other repositories in its
existing installations, so its custody remains security-critical. `release`
accepts only `v*` tags and holds Apple signing/notary credentials, the AUR SSH key, and
the dedicated Homebrew and central-archive App keys. `release-sparkle` accepts
only `v*` tags and holds the app-specific Sparkle key and bucket-scoped R2
credentials. The first stable release
and the subsequent KnxMonitor deprecation require Fabian's separate release
instruction. Static checks and PR packaging runs cover only the non-publishing
path. Local macOS visual checks cover the connection workflow, capture layout,
and ETS import; they do not establish full cross-platform visual qualification.

## Debian installation qualification

Candidate pull requests and the production release use the same
`scripts/release/qualify-deb.sh` installer on amd64 and arm64. It checks package
identity and architecture before installation, then the installed revision,
executable version, and every bundled licence notice. Minimal Ubuntu images
exclude most of `/usr/share/doc` by default. The installer passes a narrowly
scoped dpkg path inclusion for `/usr/share/doc/devknx/*`, so those checks inspect
the package's documentation rather than the container's stripping policy.

The first production attempt exposed this image-policy mismatch before public
staging; the package itself contained the notices. PR #54 moved the actual
installation check into candidate qualification as well. Local Ubuntu 24.04,
Debian 12 and Debian 13 probes passed; an isolated package with its OFL notice
removed was rejected even though its binary installed and ran.

## First-version tag exception

On 2026-09-30, Fabian explicitly authorized a one-off exception for the
unpublished first version. The failed attempt had not uploaded release assets
or published any package channel. After PR #54 passed CI and candidate
qualification, `v0.1.0` was corrected from `b674299` to
`d0a0c1308d6258a1e7cbfe7b799aa72458996a7a` and dispatched again. The tag
ruleset excluded only that exact ref during the correction; its complete
original protection was restored immediately afterward. This is not permission
to move published or future release tags.

Local packaging probes:

```sh
python3 -m unittest discover -s scripts/release -p 'test_*.py' -v
actionlint .github/workflows/release-candidate.yml .github/workflows/release.yml
```
