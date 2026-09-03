#!/usr/bin/env bash
set -euo pipefail

tools_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=lib/library-env.sh
. "$tools_dir/lib/library-env.sh"

library_root_arg=""
case "${1:-}" in
  --root)
    library_root_arg="${2:-}"
    if [[ -z "$library_root_arg" ]]; then
      printf '%s\n' '--root requires a directory' >&2
      exit 2
    fi
    ;;
  -h|--help)
    printf 'Usage: %s [--root DIR]\n' "${0##*/}"
    exit 0
    ;;
  "")
    ;;
  *)
    printf 'Unknown option: %s\n' "$1" >&2
    printf 'Usage: %s [--root DIR]\n' "${0##*/}" >&2
    exit 2
    ;;
esac

library_root="$(resolve_library_root "$library_root_arg")"
genres_dir="$library_root/genres"

if [[ ! -d "$genres_dir" ]]; then
    printf 'Genre directory not found: %s\n' "$genres_dir" >&2
    exit 1
fi

if [[ "$(id -u)" -ne 0 ]]; then
    exec sudo -- "$0" --root "$library_root"
fi

if [[ -z "${LIBRARY_USER:-}" && -z "${SUDO_USER:-}" ]]; then
    printf '%s\n' 'Set LIBRARY_USER to the unprivileged library owner.' >&2
    exit 1
fi
target_user=${LIBRARY_USER:-$SUDO_USER}
if ! id "$target_user" >/dev/null 2>&1; then
    printf 'Target user does not exist: %s\n' "$target_user" >&2
    exit 1
fi
target_group=$(id -gn "$target_user")

directory_count=$(find "$genres_dir" -type d -user root -print | wc -l)

if [[ "$directory_count" -eq 0 ]]; then
    printf '%s\n' 'No root-owned genre directories need adjustment.'
    exit 0
fi

find "$genres_dir" -type d -user root \
    -exec chown "$target_user:$target_group" {} +

printf 'Changed %s root-owned genre directories to %s:%s.\n' \
    "$directory_count" "$target_user" "$target_group"
