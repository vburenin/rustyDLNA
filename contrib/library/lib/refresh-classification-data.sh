#!/usr/bin/env bash
set -euo pipefail

lib_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
tools_dir="$(CDPATH= cd -- "$lib_dir/.." && pwd -P)"
# shellcheck source=library-env.sh
. "$lib_dir/library-env.sh"

library_root_arg=""
data_only=false

usage() {
  printf 'Usage: %s [--root DIR] [--data-only]\n' "${0##*/}"
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
    --data-only)
      data_only=true
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
export RUSTY_DLNA_LIBRARY_ROOT="$library_root"
data_dir="$(library_state_dir "$library_root")"
mkdir -p -- "$data_dir"

refresh_file() {
  local filename="$1"
  local destination="$data_dir/$filename"
  local temporary="$destination.download"
  local url="https://datasets.imdbws.com/$filename"
  local status

  if [[ -f "$destination" ]]; then
    status="$(
      curl -L --silent --show-error --fail --retry 3 \
        --remote-time --time-cond "$destination" \
        --output "$temporary" --write-out '%{http_code}' "$url"
    )"
  else
    status="$(
      curl -L --silent --show-error --fail --retry 3 \
        --remote-time --output "$temporary" \
        --write-out '%{http_code}' "$url"
    )"
  fi

  case "$status" in
    200)
      mv -- "$temporary" "$destination"
      printf 'updated: %s\n' "$filename"
      ;;
    304)
      rm -f -- "$temporary"
      printf 'current: %s\n' "$filename"
      ;;
    *)
      rm -f -- "$temporary"
      printf 'Unexpected HTTP status %s for %s\n' "$status" "$url" >&2
      return 1
      ;;
  esac
}

refresh_file title.basics.tsv.gz
refresh_file title.akas.tsv.gz
refresh_file title.episode.tsv.gz

"$lib_dir/imdb_index.py" \
  --root "$library_root" \
  --imdb-data "$data_dir/title.basics.tsv.gz" \
  --imdb-akas "$data_dir/title.akas.tsv.gz"

# Record a successful remote check, including an all-current HTTP 304 result.
# update.sh uses this timestamp to avoid checking IMDb more than once a week.
touch -- "$data_dir/.classification-refresh-stamp"

if ! "$data_only"; then
  "$lib_dir/build-genre-links.py" \
    --root "$library_root" \
    --imdb-data "$data_dir/title.basics.tsv.gz" \
    --imdb-akas "$data_dir/title.akas.tsv.gz"
  "$lib_dir/build-year-links.py" \
    --root "$library_root" \
    --imdb-data "$data_dir/title.basics.tsv.gz" \
    --imdb-akas "$data_dir/title.akas.tsv.gz" \
    --imdb-episodes "$data_dir/title.episode.tsv.gz"
fi
