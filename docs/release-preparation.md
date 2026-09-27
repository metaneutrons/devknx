# Release preparation

The release-candidate workflow is a non-publishing qualification step. It runs
on packaging pull requests and can also be dispatched manually. Its token has
read-only repository access. It creates no tag, GitHub Release, package-channel
update, signature, or notarization submission.

Its short-lived CI artifacts contain seven target-specific CLI archives, an
unsigned macOS ARM64 app ZIP, and two Debian packages:

| Target | Candidate payloads |
| --- | --- |
| macOS ARM64 | CLI `.tar.gz`, unsigned `.app.zip` |
| Linux x86_64/ARM64 GNU | CLI `.tar.gz`, `.deb` |
| Linux x86_64/ARM64 musl | headless CLI `.tar.gz` |
| Windows x86_64/ARM64 | CLI `.zip` |

The GNU and macOS archives carry the GUI-capable build. The musl builds are
headless. Every CLI archive includes the GPL licence and the embedded-font
licence notices; the app bundles the notices under `Contents/Resources`.
The packaging script rejects a mismatched executable format or architecture,
an invalid tag, missing notices, and an overwrite. It normalizes archive
ordering and timestamps from the commit epoch. The workflow extracts every
archive and runs its binary before retaining it as a CI artifact.

Candidate artifacts are **not** ready for distribution. M6 still requires
Developer ID signing, notarization and stapling of the app; SBOMs, cosign
signatures, GitHub attestations, checksums, isolated smoke tests, channel
preflight and installation tests, staged GitHub release publication, and
byte-for-byte channel read-back. The first stable release and the subsequent
KnxMonitor deprecation require Fabian's separate release instruction.

Local packaging probes:

```sh
python3 -m unittest discover -s scripts/release -p 'test_*.py' -v
actionlint .github/workflows/release-candidate.yml
```
