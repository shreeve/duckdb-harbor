# Releases and in-app updates

DuckTable ships as an app signed with a Developer ID and notarized by Apple,
installed with Homebrew from `shreeve/homebrew-tap` or with the one-line
installer, and updated in place through [Sparkle](https://sparkle-project.org),
with GitHub Releases as the only host. Nothing else runs: no server, no bucket,
no domain, no CI step. The repository must stay public: Homebrew, the installer
and Sparkle download release files without signing in.

## The pieces

| Piece | Where | Does |
| --- | --- | --- |
| Install | `shreeve/homebrew-tap` → `Casks/ducktable.rb`; `scripts/install.sh` | `brew install --cask shreeve/tap/ducktable` downloads the release's `DuckTable-X.Y.Z.zip`; `auto_updates true` leaves updating to Sparkle, and `livecheck` reads the same feed. The installer fetches the release's `DuckTable.zip` |
| Build | `scripts/sparkle.sh`, `scripts/macos-app.sh` | Fetches a pinned Sparkle by checksum into `.ducktable-cache/`, embeds it at `Contents/Frameworks`, and signs the framework and the app with the Developer ID and the hardened runtime |
| Plist keys | `scripts/macos-app.sh` | `SUFeedURL` (the feed below) and `SUPublicEDKey` (from `assets/sparkle-public-key.txt`); `CFBundleVersion` is the workspace version, which is what Sparkle orders updates by |
| Runtime | `crates/ducktable/src/updater.rs` | Loads the embedded framework, starts `SPUUpdater` with Sparkle's standard user driver, forwards the menu item, and holds Install and Relaunch until the quit dialog has asked whatever ⌘Q would ask (EDITING.md, "Dialogs") |
| Menu | `crates/ducktable/src/main.rs` | DuckTable → Check for Updates…, present only when the updater started |
| Feed | `scripts/appcast.sh` | Signs the archives in a directory and writes its `appcast.xml`, embedding each version's notes |
| Release | `scripts/release.sh` | Stamps the version, builds, notarizes and staples, zips, signs the feed, drafts the release, commits, tags, pushes, publishes, and refreshes the feed release; undoes itself when a step fails |
| Cask | `scripts/update-cask.sh` | Writes the cask for a published version with the archive's sha256 and opens the tap's pull request |

## The feed release

Sparkle needs one URL that always serves the current appcast, and GitHub's
per-version release URLs contain the tag. So one extra release,
**`ducktable-updates`**, never changes name and holds:

- `appcast.xml`, the feed, rewritten every release;
- `DuckTable-<version>.zip` for every version on the feed, since the appcast's
  enclosure URLs are this release's download URLs;
- `*.delta` files, binary deltas from the two previous versions.

It is a prerelease and never `--latest`, so `/releases/latest` stays harbor's
and `install.sh`, which resolves `ducktable-v*` tags by name, never sees it.
The versioned `ducktable-v*` releases carry `DuckTable-X.Y.Z.zip` for the cask
and `DuckTable.zip` for the installer and for people.

Two signatures, for two jobs:

- **Gatekeeper** judges a file marked as downloaded from the internet, as
  Homebrew and browsers mark it, on first launch. It accepts an app signed with
  a Developer ID and notarized: Apple has scanned it and issued a ticket, which
  the release staples into the bundle so the check works offline.
- **Sparkle** accepts an update when its code signature is valid and its EdDSA
  signature matches `SUPublicEDKey`.

Old archives can be deleted from the feed release whenever it grows tiresome;
Sparkle only needs the newest item, plus whichever versions should still get a
delta.

## One-time setup

### 1. The Developer ID and notarization

Releases are signed with the Developer ID Application certificate of the
individual team `SD6N7Z8P9P`, the same one local builds use and the one Shotts,
Transfer and Lyte ship with. Its private key lives in the login keychain; to
release from another Mac, export the certificate with its key from Keychain
Access and import it there. Notarization signs in through the notarytool
keychain profile `notary-tool`, stored once per Mac and shared with those apps.

```sh
security find-identity -v -p codesigning   # must list "Developer ID Application: Steve Shreeve (SD6N7Z8P9P)"
xcrun notarytool store-credentials notary-tool   # Apple ID, an app-specific password, team SD6N7Z8P9P
xcrun notarytool history --keychain-profile notary-tool
```

A Mac without the certificate builds with `SIGN=- scripts/macos-app.sh`, ad
hoc and without the hardened runtime; such a bundle runs where it was built
but cannot be released.

### 2. The update signing key

Updates are signed with an ed25519 key of DuckTable's own, under the keychain
account `ducktable` (Shotts uses `shotts`, Transfer the default account and
Lyte `lyte`; never mix them, and never delete or export a key by service alone,
which would take them all). The private half lives in the login keychain and,
as a backup, in the gitignored `notes.txt` at the repository root, which is
never committed. The public half is `assets/sparkle-public-key.txt`, which the
bundle carries as `SUPublicEDKey`. The tools land in
`.ducktable-cache/sparkle/<version>/bin` after any `scripts/macos-app.sh` run.

```sh
bin=.ducktable-cache/sparkle/2.10.0/bin
$bin/generate_keys --account ducktable -p                       # prints the public key; it must equal assets/sparkle-public-key.txt
$bin/generate_keys --account ducktable -x /tmp/ducktable-key    # exports the private key for a backup
$bin/generate_keys --account ducktable -f /tmp/ducktable-key    # imports it on another Mac
rm /tmp/ducktable-key
```

Losing the private key strands every installed copy on its version, because an
app only trusts the key it shipped with. Anyone who has it can sign an update
every installed copy will accept.

## The bundle's identity

`scripts/macos-app.sh` signs the app with `--identifier com.shreeve.ducktable`,
the same string as `CFBundleIdentifier`, and fails the build unless
`codesign -dv` reports it for both `DuckTable.app` and
`Contents/MacOS/ducktable`. Sparkle's nested code is signed first and keeps Sparkle's own
identifiers. The identifier is a label, not a credential. macOS files privacy
decisions under it — Local Network above all, which DuckTable needs for SSH
tunnels to hosts on the LAN — so holding it fixed keeps one row in System
Settings attached to the app through every update. The bundle's
`NSLocalNetworkUsageDescription` is the sentence macOS shows when it asks.

Every version signs with the same Developer ID, so its designated requirement
— the identifier and the team — is the same from version to version. Sparkle
trusts an update by its EdDSA signature against the public key in the
installed copy, and separately requires that the update's code signature be
valid.

Two rules keep an installed copy's identity intact: never re-sign or rename
it, and never copy over it in place, since macOS caches an executable's
signature by inode and kills the next launch of a file overwritten under it.
`scripts/install.sh` and `scripts/install-local.sh` stage the bundle beside
the destination, rename the installed app aside, rename the staged one in,
remove the old one last (putting it back if the rename fails), and register
the result with Launch Services (`lsregister -f`). Sparkle replaces the
bundle whole on update, which is the same kind of swap.

## Cutting a release

First land the version's changelog section, `## X.Y.Z — <date>` in
`CHANGELOG.md`, and the Sparkle pin if Sparkle has a newer stable release: the
section becomes the GitHub release notes and the notes Sparkle shows in the
update dialog, and the script refuses a release without one, or with a stale
pin. When harbor ships too, release it first, so the lockfile records its new
version. Then, from `main`, clean and in step with `origin/main`:

```sh
scripts/release.sh X.Y.Z --notes     # prints the notes it would publish, and nothing else
scripts/release.sh X.Y.Z --dry-run   # builds and notarizes under target/release-X.Y.Z; publishes nothing
scripts/release.sh X.Y.Z             # releases
scripts/update-cask.sh X.Y.Z         # opens the tap's pull request; merge it
```

The release script sets `version` in `Cargo.toml` and runs `cargo update -w`,
builds the bundle signed with a secure timestamp, sends it to Apple, staples
the ticket, and checks that Gatekeeper accepts it. It zips the stapled app as
`DuckTable-X.Y.Z.zip` and `DuckTable.zip`, downloads the feed release, adds the
archive and its notes, and runs `scripts/appcast.sh`. Then it drafts the
`ducktable-vX.Y.Z` release, commits "DuckTable X.Y.Z", tags it (annotated),
pushes `main` and the tag together, publishes the release (never `--latest`),
and uploads the feed back with `--clobber`. A dry run stops before the draft,
with the repository as it found it, and still sends the app to Apple, which
makes nothing public.

It refuses to run unless the keychain holds the Developer ID, the notary
profile signs in, and the keychain's `ducktable` update key matches
`assets/sparkle-public-key.txt`. A real release also refuses unless it is on a
clean `main` in step with `origin/main`, `gh` is signed in, the repository is
public, no tag or release for the version exists, the version is higher than
the latest `ducktable-v*` tag, and `CHANGELOG.md` has its section. Installed
copies pick the version up on their next scheduled check, once a day, or on
Check for Updates.

## Trying it locally

Debug builds keep the updater dormant so the dev bundle never offers to
replace itself. To exercise the real flow from one:

```sh
scripts/macos-app.sh
DUCKTABLE_FORCE_UPDATER=1 open target/DuckTable.app
```

Check for Updates then talks to the real feed. A first launch of any bundle
also gets Sparkle's one-time prompt asking whether to check automatically;
the answer is stored in the app's defaults under `com.shreeve.ducktable`.

To rehearse an actual update without publishing, build two bundles at
different versions, run `scripts/appcast.sh` over a directory holding the
newer one as `DuckTable-<version>.zip`, serve that directory with any static
server, and point the older bundle at it by editing `SUFeedURL` in its
Info.plist before signing.
