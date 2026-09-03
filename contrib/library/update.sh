#!/usr/bin/env bash
set -euo pipefail

# Provider credentials must come from the environment. Do not export API keys
# or tokens from this file. Supported variables:
#   TMDB_API_TOKEN, TMDB_API_KEY, OMDB_API_KEYS, OMDB_API_KEY

tools_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=lib/library-env.sh
. "$tools_dir/lib/library-env.sh"

library_root_arg=""
dry_run=false
refresh_mode=auto
refresh_interval_days=7
refresh_interval_seconds=$((refresh_interval_days * 24 * 60 * 60))
audit_all=false
refresh_tmdb=false
refresh_wikidata=false

usage() {
  cat <<EOF
Usage: ${0##*/} [--root DIR] [--dry-run] [--no-refresh|--refresh-data]
                [--refresh-tmdb] [--refresh-wikidata] [--all]

Refresh classification data when its last successful check is at least seven
days old; rebuild genre, BY_YEAR, BY_AGE, UNTIL_AGE, and BY_RATING links; fill
movie descriptions and missing rustyDLNA posters; remove broken links; and
report unclassified videos.

The media library is --root, or RUSTY_DLNA_LIBRARY_ROOT, LIBRARY_ROOT, or
RUSTY_DLNA_MEDIA. Caches and IMDb dumps live in <library>/.rusty-library/.
TMDB/OMDb credentials are read from the environment and are never stored here.

  --root DIR     Media library root
  --dry-run      Preview the rebuild, poster fetch, and cleanup without changing anything
  --no-refresh   Use the existing local IMDb datasets regardless of their age
  --refresh-data Force an IMDb dataset refresh check on this run
  --refresh-tmdb Refetch cached TMDB metadata (requires TMDB_API_TOKEN)
  --refresh-wikidata
                 Refetch cached Wikidata age certifications
  --all          Include shows, duplicates, incomplete/review areas, and sport
                 in the final unclassified-video audit
EOF
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
    --no-refresh)
      if [[ "$refresh_mode" == always ]]; then
        printf '%s\n' '--no-refresh and --refresh-data cannot be used together.' >&2
        exit 2
      fi
      refresh_mode=never
      ;;
    --refresh-data)
      if [[ "$refresh_mode" == never ]]; then
        printf '%s\n' '--no-refresh and --refresh-data cannot be used together.' >&2
        exit 2
      fi
      refresh_mode=always
      ;;
    --refresh-tmdb)
      refresh_tmdb=true
      ;;
    --refresh-wikidata)
      refresh_wikidata=true
      ;;
    --all)
      audit_all=true
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'Unknown option: %s\n\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

library_root="$(resolve_library_root "$library_root_arg")"
export RUSTY_DLNA_LIBRARY_ROOT="$library_root"
state_dir="$(library_state_dir "$library_root")"
refresh_stamp="$state_dir/.classification-refresh-stamp"
mkdir -p -- "$state_dir"

classification_refresh_due() {
  local dataset dataset_mtime checked_at now

  checked_at=""
  for dataset in title.basics.tsv.gz title.akas.tsv.gz title.episode.tsv.gz; do
    if [[ ! -f "$state_dir/$dataset" ]]; then
      return 0
    fi
    if ! dataset_mtime="$(stat -c %Y -- "$state_dir/$dataset")"; then
      return 0
    fi
    if [[ -z "$checked_at" ]] || ((dataset_mtime < checked_at)); then
      checked_at="$dataset_mtime"
    fi
  done
  if [[ -f "$refresh_stamp" ]]; then
    if ! checked_at="$(stat -c %Y -- "$refresh_stamp")"; then
      return 0
    fi
  fi
  now="$(date +%s)"
  ((now - checked_at >= refresh_interval_seconds))
}

if command -v flock >/dev/null 2>&1; then
  exec 9>"$state_dir/.update.lock"
  if ! flock -n 9; then
    printf 'Another genre update is already running.\n' >&2
    exit 1
  fi
fi

if "$dry_run"; then
  if [[ "$refresh_mode" == always ]]; then
    printf '%s\n\n' 'Note: --refresh-data is ignored during a dry run.'
  elif [[ "$refresh_mode" == auto ]] && classification_refresh_due; then
    printf 'Note: the weekly IMDb refresh is due; a dry run will not perform it.\n\n'
  fi
  printf '== Preview genre-link rebuild ==\n'
  build_args=(--root "$library_root" --dry-run)
  if "$refresh_tmdb"; then
    printf '%s\n' 'Note: --refresh-tmdb is ignored during a dry run.'
  fi
  if "$refresh_wikidata"; then
    printf '%s\n' 'Note: --refresh-wikidata is ignored during a dry run.'
  fi
  "$tools_dir/lib/build-genre-links.py" "${build_args[@]}"

  printf '\n== Preview BY_YEAR rebuild ==\n'
  "$tools_dir/lib/build-year-links.py" --root "$library_root" --dry-run

  printf '\n== Preview BY_AGE, UNTIL_AGE, and BY_RATING rebuild ==\n'
  "$tools_dir/lib/build-age-links.py" --root "$library_root" --dry-run

  printf '\n== Preview movie-description sidecars ==\n'
  "$tools_dir/fetch-movie-descriptions.py" --root "$library_root" --dry-run

  printf '\n== Preview broken-link cleanup ==\n'
  "$tools_dir/clean-dead-links.sh" --root "$library_root" --dry-run

  printf '\n== Preview rustyDLNA poster fetch ==\n'
  "$tools_dir/fetch-dlna-artwork.py" --root "$library_root" --dry-run
else
  refresh_now=false
  if [[ "$refresh_mode" == always ]]; then
    refresh_now=true
  elif [[ "$refresh_mode" == auto ]] && classification_refresh_due; then
    refresh_now=true
  fi

  if "$refresh_now"; then
    printf '== Refresh IMDb classification data ==\n'
    "$tools_dir/lib/refresh-classification-data.sh" --root "$library_root" --data-only
    printf '\n'
  elif [[ "$refresh_mode" == never ]]; then
    printf 'Using cached IMDb classification data (--no-refresh).\n\n'
  else
    printf 'Using cached IMDb classification data (checked within the last %d days).\n\n' \
      "$refresh_interval_days"
  fi

  printf '== Rebuild genre links ==\n'
  build_args=(--root "$library_root")
  if "$refresh_tmdb"; then
    build_args+=(--refresh-tmdb)
  fi
  "$tools_dir/lib/build-genre-links.py" "${build_args[@]}"

  printf '\n== Rebuild BY_YEAR links ==\n'
  "$tools_dir/lib/build-year-links.py" --root "$library_root"

  printf '\n== Rebuild BY_AGE, UNTIL_AGE, and BY_RATING links ==\n'
  age_args=(--root "$library_root")
  if "$refresh_tmdb"; then
    age_args+=(--refresh-tmdb)
  fi
  if "$refresh_wikidata"; then
    age_args+=(--refresh-wikidata)
  fi
  "$tools_dir/lib/build-age-links.py" "${age_args[@]}"

  printf '\n== Fill movie-description sidecars ==\n'
  description_args=(--root "$library_root")
  if "$refresh_tmdb"; then
    description_args+=(--refresh)
  fi
  "$tools_dir/fetch-movie-descriptions.py" "${description_args[@]}"

  printf '\n== Remove broken generated-view links ==\n'
  "$tools_dir/clean-dead-links.sh" --root "$library_root"

  printf '\n== Fill missing rustyDLNA posters ==\n'
  set +e
  "$tools_dir/fetch-dlna-artwork.py" --root "$library_root"
  art_status=$?
  set -e
  if ((art_status != 0)); then
    printf '%s\n' 'Some posters could not be fetched. Report remaining gaps; do not invent artwork.'
  fi
fi

printf '\n== Videos without genre links ==\n'
if "$audit_all"; then
  "$tools_dir/find-unclassified-videos.py" --root "$library_root" --all
else
  "$tools_dir/find-unclassified-videos.py" --root "$library_root"
fi

printf '\n== Dolby Vision Profile 7 (Google Streamer incompatible) ==\n'
set +e
"$tools_dir/find-dv-profile7.py" --root "$library_root"
p7_status=$?
set -e
if ((p7_status != 0)); then
  printf '%s\n' 'Profile 7 remuxes remain in the playback catalog.' \
    'Do not recode them unless explicitly asked.' \
    'When asked, use recode-dv-profile7.py, keep the HDR10 Streamer MP4 in the' \
    'catalog, and archive the remux under to-review/P7-Recoded-For-Streamer/.'
fi
