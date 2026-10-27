# Updater verification

Portable UsageHub builds query only
`https://api.github.com/repos/rostikowb/UsageHub/releases/latest`, using the
repository compiled into `Cargo.toml`. The in-app check offers newer stable
versions; an equal or older release is up to date. Installing an available
release requires confirmation. GitHub/network/metadata errors remain errors;
there is no fallback to CodeZeno's repository or binary.

There is no UsageHub WinGet listing. Executables in recognized WinGet package
directories report that unsupported channel instead of querying or upgrading
the upstream package.

The portable updater accepts only the exact `usagehub.exe` asset from this
fork's latest stable release. It rejects duplicate assets, foreign or upstream
download URLs, invalid semantic versions, drafts, and prereleases (including
prerelease tags incorrectly marked as stable).
Version precedence follows SemVer: `2.14.0-beta1` is below `2.14.0`, and build
metadata does not affect ordering.

Every update requires a valid `sha256:` digest and a nonzero size of at most
100 MiB in the GitHub release asset metadata. The updater streams the download,
checks its actual byte count and SHA-256, and only promotes the temporary file
after both match. It stops reading after the expected size plus one byte,
regardless of Content-Length, and deletes its partial file on failure. Requests
and redirects require HTTPS.

The helper receives the expected size and digest, rechecks the staged file before
replacing the installed executable, and holds a Windows file handle that denies
writes and deletion through replacement. Missing or malformed verification
arguments are errors; the old helper invocation without a digest is rejected.

## Trust boundary

The digest comes from the authenticated GitHub API over TLS. This detects corrupt
or substituted downloads relative to that metadata. It is **not a publisher
signature** and does not protect against a compromised GitHub repository or
release account. There is no Authenticode or detached-signature verification in
this updater; adding it requires release signing and a trusted publisher key.
Code already running as the same Windows user is outside this trust boundary.

GitHub computes asset digests for uploaded release assets; see the
[GitHub release asset API](https://docs.github.com/en/rest/releases/assets).
Releases without a digest cannot be installed through the portable updater.
The fork's release workflow does not need a separate checksum upload.

## Tests

Run `cargo test --locked updater` on Windows. Tests exercise metadata selection,
SemVer precedence, streamed size limits, known SHA-256 vectors, corrupt and
truncated bodies, interrupted and failed I/O, partial-file cleanup, helper
arguments, and Windows file locking. Tests do not download or execute updates.
The release workflow runs these tests before building a release.
Channel tests cover own-fork source isolation and source errors, including the
unsupported WinGet path. A live check requires a published stable UsageHub
release with its executable asset; the old fork build with updates disabled
must be replaced manually once.

## Release and live smoke

Publish only `usagehub.exe` at a matching stable version tag in
`rostikowb/UsageHub`, never profiles, tokens, settings, logs, or backups. The
tag workflow uses explicit MSVC host/target settings on `windows-latest`; a
manual local release uses the documented gnullvm build. Do not silently
replace an already-published asset with a different build.

After publishing, compare the API asset's nonzero `size` and `sha256:` `digest`
with the built executable. Run the installed same-version app and use **Check
for updates**: it should report up to date, not disabled or a missing release.
This proves the live metadata/check path, not a full version-to-version
replacement; exercise the latter on a disposable owned installation before
claiming it. Normal shutdown requests close the existing dashboard before the
helper replaces the mapped executable.
