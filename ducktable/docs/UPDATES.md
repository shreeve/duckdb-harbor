# In-app updates

DuckTable updates itself through [Sparkle](https://sparkle-project.org), the
framework most self-updating Mac apps use, with GitHub Releases as the only
host. Nothing else runs: no server, no bucket, no domain.

## The pieces

| Piece | Where | Does |
| --- | --- | --- |
| Framework | `scripts/sparkle.sh`, `scripts/macos-app.sh` | Fetches a pinned Sparkle by checksum into `.ducktable-cache/`, embeds it at `Contents/Frameworks`, ad-hoc signs it with the app |
| Plist keys | `scripts/macos-app.sh` | `SUFeedURL` (the feed below) and `SUPublicEDKey` (from `assets/sparkle-public-key.txt`); `CFBundleVersion` is the workspace version, which is what Sparkle orders updates by |
| Runtime | `crates/ducktable/src/updater.rs` | Loads the embedded framework, starts `SPUUpdater` with Sparkle's standard user driver, forwards the menu item |
| Menu | `crates/ducktable/src/main.rs` | DuckTable → Check for Updates…, present only when the updater started |
| Feed | `scripts/appcast.sh` | Signs the archives in a directory and writes its `appcast.xml` |
| Publish | `.github/workflows/DuckTableRelease.yml` | After the versioned release, refreshes the feed release |

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
The versioned `ducktable-v*` releases keep their `DuckTable.zip` for the
installer and for people.

Old archives can be deleted from the feed release whenever it grows tiresome;
Sparkle only needs the newest item, plus whichever versions should still get a
delta.

## One-time setup

### 1. The signing key

Updates are signed with an ed25519 key. The private half lives in three
places: the login keychain under the account `ducktable`, the
`SPARKLE_PRIVATE_KEY` repository secret for CI, and the gitignored `notes.txt`
at the repository root, which is never committed. The public half ships in the
bundle. The tools land in
`.ducktable-cache/sparkle/<version>/bin` after any `scripts/macos-app.sh` run.

```sh
bin=.ducktable-cache/sparkle/2.10.0/bin
$bin/generate_keys --account ducktable          # creates the key, prints the public half
$bin/generate_keys --account ducktable -p > assets/sparkle-public-key.txt
$bin/generate_keys --account ducktable -x /tmp/ducktable-sparkle-key.txt
gh secret set SPARKLE_PRIVATE_KEY --repo shreeve/duckdb-harbor < /tmp/ducktable-sparkle-key.txt
rm /tmp/ducktable-sparkle-key.txt
```

Commit `assets/sparkle-public-key.txt`. Back the private key up somewhere
durable: lose it and every installed copy is stranded on its version forever,
because an app only trusts the key it shipped with.

### 2. Nothing else

No Developer ID, no notarization. The bundle is ad-hoc signed;
Sparkle verifies that an update's signature is valid and its EdDSA signature
matches, and both hold for an ad-hoc bundle. The installer avoids
Gatekeeper by never being quarantined.

## The bundle's identity

`scripts/macos-app.sh` signs the app with `--identifier com.shreeve.ducktable`,
the same string as `CFBundleIdentifier`, and fails the build unless
`codesign -dv` reports it for both `DuckTable.app` and
`Contents/MacOS/ducktable`; the release workflow's smoke test asserts it
again. Sparkle's nested code is signed first and keeps Sparkle's own
identifiers. The identifier is a label, not a credential. macOS files privacy
decisions under it — Local Network above all, which DuckTable needs for SSH
tunnels to hosts on the LAN — so holding it fixed keeps one row in System
Settings attached to the app through every update. The bundle's
`NSLocalNetworkUsageDescription` is the sentence macOS shows when it asks.

An ad-hoc signature's designated requirement is its own code hash, so no two
versions ever match each other by code signing, whatever they are named.
Sparkle therefore trusts an update by its EdDSA signature against the public
key in the installed copy, and separately requires that the update's code
signature be valid; the identifier takes no part in either check.

Two rules keep an installed copy's identity intact: never re-sign or rename
it, and never copy over it in place, since macOS caches an executable's
signature by inode and kills the next launch of a file overwritten under it.
`scripts/install.sh` and `scripts/install-local.sh` stage the bundle beside
the destination, rename the installed app aside, rename the staged one in,
remove the old one last (putting it back if the rename fails), and register
the result with Launch Services (`lsregister -f`). Sparkle replaces the
bundle whole on update, which is the same kind of swap.

## Cutting a release

The steps are in the repository's `HANDOFF.md`, under Releasing: bump `version`
in `Cargo.toml` and run `cargo update -w`, add the changelog entry, bump the
Sparkle pin, prove the bundle with `scripts/macos-app.sh release`, and push an
annotated `ducktable-vX.Y.Z` tag. The workflow builds the bundle, creates the
versioned release, then downloads the feed release, adds
`DuckTable-X.Y.Z.zip`, runs `scripts/appcast.sh`, and uploads the result back
with `--clobber`. Installed
copies pick it up on their next scheduled check, once a day, or on Check for
Updates.

The workflow fails, rather than publishing a feed nobody can verify, if the
secret is missing or does not match the public key in the bundle.

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
