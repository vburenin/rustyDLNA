# Shared library-root helpers for contrib/library shell programs.
# Source this file after setting tools_dir to the contrib/library directory.

library_root_from_env() {
  if [[ -n "${1:-}" ]]; then
    printf '%s\n' "$1"
    return 0
  fi
  if [[ -n "${RUSTY_DLNA_LIBRARY_ROOT:-}" ]]; then
    printf '%s\n' "$RUSTY_DLNA_LIBRARY_ROOT"
    return 0
  fi
  if [[ -n "${LIBRARY_ROOT:-}" ]]; then
    printf '%s\n' "$LIBRARY_ROOT"
    return 0
  fi
  if [[ -n "${RUSTY_DLNA_MEDIA:-}" ]]; then
    printf '%s\n' "$RUSTY_DLNA_MEDIA"
    return 0
  fi
  printf 'error: set --root or RUSTY_DLNA_MEDIA to the media library\n' >&2
  return 2
}

resolve_library_root() {
  local candidate
  candidate="$(library_root_from_env "${1:-}")" || return
  if [[ ! -d "$candidate" ]]; then
    printf 'error: library root is not a directory: %s\n' "$candidate" >&2
    return 2
  fi
  (CDPATH= cd -- "$candidate" && pwd -P)
}

library_state_dir() {
  printf '%s/.rusty-library\n' "$1"
}
