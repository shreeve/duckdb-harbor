#!/usr/bin/env bash
#
# release.sh — turn a finished CI build into the assets a release carries.
#
#   test/scripts/release.sh                      stage assets for the version in Cargo.toml
#   test/scripts/release.sh --publish --notes N   ...and create the release from N
#   test/scripts/release.sh --version 0.8.2 --run 31763866157
#
# The distribution pipeline builds one set of binaries — the stable DuckDB, one
# per platform — and uploads them as workflow artifacts. It does not publish
# anything. Everything between that and a release page used to be done by hand:
# download, restamp for the preview DuckDB, zip ten files, checksum, upload.
# That is what this does, in order, with the checks that hand-assembly skips.
#
# What it will not do quietly:
#
#   - use a run that did not succeed, or whose binaries carry a different
#     extension version than the one being released. A stale run produces a
#     release full of the previous version's code and nothing about the file
#     names would say so.
#   - ship a platform the workflow was supposed to build. If the matrix says
#     five and four came back, that is a failed release, not a small one.
#   - publish. Staging is the default; --publish is a separate word, and it
#     needs notes, because a release page with no text is worse than no page.
#
# Versions are read from the files that already decide them — the workflow for
# the stable DuckDB, the Makefile for the preview ones — so there is one place
# to change each and this script cannot drift from the build.

set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$here"

version=""
run_id=""
notes=""
out="$here/build/release-assets"
publish=0

while (($#)); do
  case $1 in
    --version) version=${2#v}; shift 2 ;;
    --run)     run_id=$2; shift 2 ;;
    --notes)   notes=$2; shift 2 ;;
    --out)     out=$2; shift 2 ;;
    --publish) publish=1; shift ;;
    -h|--help) sed -n '3,${/^#/!q;p;}' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "release: unknown argument $1" >&2; exit 2 ;;
  esac
done

die() { echo "release: $*" >&2; exit 1; }
say() { printf '\033[1m%s\033[0m\n' "$*"; }

command -v gh   >/dev/null || die "gh is not installed"
command -v zip  >/dev/null || die "zip is not installed"

# ---------------------------------------------------------------------------
# What we are building, and against which DuckDBs
# ---------------------------------------------------------------------------

if [[ -z $version ]]; then
  version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
fi
[[ -n $version ]] || die "could not read the version from Cargo.toml"
tag="v$version"

workflow=.github/workflows/MainDistributionPipeline.yml
stable=$(sed -n 's/^ *duckdb_version: *\(v[^ ]*\)/\1/p' "$workflow" | head -1)
[[ -n $stable ]] || die "could not read duckdb_version from $workflow"

# The preview versions the Makefile already knows how to stamp for. Same list,
# so `make release_all` locally and a release built here cannot disagree.
previews=$(sed -n 's/^STAMPED_DUCKDB_VERSIONS *?*= *//p' Makefile | head -1)

say "harbor $tag — stable $stable, previews: ${previews:-none}"

# ---------------------------------------------------------------------------
# The run
# ---------------------------------------------------------------------------

if [[ -z $run_id ]]; then
  run_id=$(gh run list --workflow="Main Extension Distribution Pipeline" \
                       --branch "$tag" --limit 1 --json databaseId -q '.[0].databaseId')
  [[ -n $run_id ]] || die "no distribution pipeline run for $tag — push the tag first"
fi

read -r status conclusion < <(gh run view "$run_id" --json status,conclusion \
                                 -q '[.status, .conclusion] | @tsv')
[[ $status == completed ]] || die "run $run_id is $status — wait for it to finish"
[[ $conclusion == success ]] || die "run $run_id concluded $conclusion — fix it before releasing"

# ---------------------------------------------------------------------------
# Download and check
# ---------------------------------------------------------------------------

work=$(mktemp -d "${TMPDIR:-/tmp}/harbor-release.XXXXXX")
trap 'rm -rf "$work"' EXIT

say "downloading artifacts from run $run_id"
gh run download "$run_id" --dir "$work/artifacts" >/dev/null

shopt -s nullglob
built=("$work"/artifacts/harbor-"$stable"-extension-*)
((${#built[@]})) || die "run $run_id has no harbor-$stable-extension-* artifacts"

# Every platform the workflow was configured to build has to be present. The
# matrix is authoritative, not the download: a job that failed leaves no
# artifact, and a release quietly missing a platform is the failure mode this
# check exists for.
expected=$(gh run view "$run_id" --json jobs \
             -q '[.jobs[] | select(.name | test("\\(")) | .name | capture("\\((?<p>[a-z0-9_]+),").p] | length')
if [[ -n $expected ]] && ((expected != ${#built[@]})); then
  die "the matrix built $expected platforms but only ${#built[@]} artifacts came back"
fi

rm -rf "$out"; mkdir -p "$out" "$work/stage"

platforms=()
for dir in "${built[@]}"; do
  platform=${dir##*-extension-}
  ext="$dir/harbor.duckdb_extension"
  [[ -f $ext ]] || die "$platform: the artifact has no harbor.duckdb_extension"

  # The trailer is the only thing that says what this binary is. Read it rather
  # than trust the artifact's name: the name comes from the workflow inputs and
  # would look right even if the build had been made from another commit.
  stamped_ext=$(python3 -c "
import sys
d = open(sys.argv[1], 'rb').read()
print(d[-384:-352].rstrip(b'\x00').decode(), d[-352:-320].rstrip(b'\x00').decode(),
      d[-320:-288].rstrip(b'\x00').decode())
" "$ext")
  read -r got_version got_duckdb got_platform <<<"$stamped_ext"
  [[ $got_version == "$tag" ]] || die "$platform: built as $got_version, releasing $tag — stale run?"
  [[ $got_duckdb == "$stable" ]] || die "$platform: built for DuckDB $got_duckdb, expected $stable"
  [[ $got_platform == "$platform" ]] || die "$platform: the binary says $got_platform"

  platforms+=("$platform")

  pack() { # pack <duckdb-version> <source>
    rm -f "$work/stage/harbor.duckdb_extension"
    cp "$2" "$work/stage/harbor.duckdb_extension"
    (cd "$work/stage" && zip -q -X "$out/harbor-$tag-duckdb-$1-$platform.zip" harbor.duckdb_extension)
  }

  pack "$stable" "$ext"
  for preview in $previews; do
    "$here/test/scripts/restamp.py" "$ext" "$preview" "$work/restamped" >/dev/null
    pack "$preview" "$work/restamped"
  done
done

# The launcher ships with the binaries: it is what finds and loads them, and a
# release whose extension has moved on without it is how a fixed launcher bug
# stays unfixed for everybody who installed by download.
cp bin/duckdb-harbor "$out/duckdb-harbor"
chmod +x "$out/duckdb-harbor"

(cd "$out" && shasum -a 256 -- * > SHA256SUMS)

say "staged in $out"
printf '  %s\n' "${platforms[@]}" | sort
echo
ls -1 "$out"

# ---------------------------------------------------------------------------
# Publish
# ---------------------------------------------------------------------------

if ((!publish)); then
  echo
  echo "Not published. Review the assets, then:"
  echo "  $0 --version $version --run $run_id --publish --notes NOTES.md"
  exit 0
fi

[[ -n $notes ]] || die "--publish needs --notes FILE"
[[ -f $notes ]] || die "no notes file at $notes"

if gh release view "$tag" >/dev/null 2>&1; then
  die "$tag is already released — delete it first, or upload to it with 'gh release upload'"
fi

say "publishing $tag"
gh release create "$tag" --title "$tag" --notes-file "$notes" "$out"/*
