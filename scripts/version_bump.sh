#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "Usage: $0 [major|minor|patch]"
  echo "       (defaults to patch if omitted)"
  exit 1
}

if [[ $# -eq 0 ]]; then
  bump_type=patch
elif [[ $# -eq 1 ]]; then
  bump_type="$1"
else
  usage
fi

case "$bump_type" in
  major|minor|patch) ;;
  *) usage ;;
esac

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KAKU_MANIFEST="$REPO_ROOT/kaku/Cargo.toml"
KAKU_GUI_MANIFEST="$REPO_ROOT/kaku-gui/Cargo.toml"

extract_version() {
  local manifest="$1"
  local version
  version="$(sed -nE 's/^version = "([0-9]+\.[0-9]+\.[0-9]+)"$/\1/p' "$manifest" | head -n1)"
  if [[ -z "$version" ]]; then
    echo "Failed to read semver version from $manifest" >&2
    exit 1
  fi
  echo "$version"
}

kaku_version="$(extract_version "$KAKU_MANIFEST")"
kaku_gui_version="$(extract_version "$KAKU_GUI_MANIFEST")"

if [[ "$kaku_version" != "$kaku_gui_version" ]]; then
  echo "Version mismatch: kaku=$kaku_version, kaku-gui=$kaku_gui_version" >&2
  echo "Please sync them before bumping." >&2
  exit 1
fi

IFS='.' read -r major minor patch <<< "$kaku_version"

case "$bump_type" in
  major)
    major=$((major + 1))
    minor=0
    patch=0
    ;;
  minor)
    minor=$((minor + 1))
    patch=0
    ;;
  patch)
    patch=$((patch + 1))
    ;;
esac

new_version="${major}.${minor}.${patch}"

update_manifest() {
  local manifest="$1"
  local tmp_file
  tmp_file="$(mktemp)"

  awk -v ver="$new_version" '
    BEGIN { updated = 0 }
    {
      if (!updated && $0 ~ /^version = "[0-9]+\.[0-9]+\.[0-9]+"$/) {
        print "version = \"" ver "\""
        updated = 1
        next
      }
      print
    }
    END {
      if (!updated) {
        exit 1
      }
    }
  ' "$manifest" > "$tmp_file"

  mv "$tmp_file" "$manifest"
}

update_manifest "$KAKU_MANIFEST"
update_manifest "$KAKU_GUI_MANIFEST"

echo "Bumped versions:"
echo "  kaku:     $kaku_version -> $new_version"
echo "  kaku-gui: $kaku_gui_version -> $new_version"
