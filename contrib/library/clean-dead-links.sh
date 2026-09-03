#!/usr/bin/env bash
set -euo pipefail

tools_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=lib/library-env.sh
. "$tools_dir/lib/library-env.sh"

library_root_arg=""
dry_run=false

usage() {
  printf 'Usage: %s [--root DIR] [--dry-run]\n' "${0##*/}"
}

while (($#)); do
  case "$1" in
    --root)
      if (($# < 2)); then
        printf '%s\n' '--root requires a directory' >&2
        exit 2
      fi
      library_root_arg="$2"
      shift
      ;;
    --dry-run)
      dry_run=true
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'Unknown option: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

library_root="$(resolve_library_root "$library_root_arg")"
genres_dir="$library_root/genres"

if [[ ! -d "$genres_dir" ]]; then
  printf 'Genre directory not found: %s\n' "$genres_dir" >&2
  exit 1
fi

removed=0
while IFS= read -r -d '' link; do
  relative_path="${link#"$genres_dir"/}"
  if "$dry_run"; then
    printf 'would remove: %s\n' "$relative_path"
  else
    rm -- "$link"
    printf 'removed: %s\n' "$relative_path"
  fi
  ((removed += 1))
done < <(find "$genres_dir" -type l ! -exec test -e {} \; -print0)

if "$dry_run"; then
  printf 'Broken symlinks found: %d\n' "$removed"
else
  printf 'Broken symlinks removed: %d\n' "$removed"
fi
