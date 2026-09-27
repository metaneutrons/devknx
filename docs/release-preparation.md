# Release preparation

The release-candidate workflow is a non-publishing qualification step. It runs
on packaging pull requests and can also be dispatched manually. Its token has
read-only repository access. It creates no tag, GitHub Release, package-channel
update, signature, or notarization submission.

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

GitHub has two isolated environments. `release-please` accepts only
`main` and holds only its dedicated App key. `release` accepts only
`v*` tags and holds Apple signing/notary credentials, the AUR SSH key, and
the dedicated Homebrew and central-archive App keys. The first stable release
and the subsequent KnxMonitor deprecation require Fabian's separate release
instruction. The pipeline cannot be claimed end-to-end qualified until a tag
run has actually completed; static checks and PR packaging runs cover only
the non-publishing path. Visual inspection of the app is also still pending.

Local packaging probes:

```sh
python3 -m unittest discover -s scripts/release -p 'test_*.py' -v
actionlint .github/workflows/release-candidate.yml .github/workflows/release.yml
```
