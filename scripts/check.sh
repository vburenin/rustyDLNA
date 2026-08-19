#!/bin/sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$ROOT"
TOOLCHAIN=$(sed -n 's/^channel = "\([^"]*\)"/\1/p' rust-toolchain.toml)
[ -n "$TOOLCHAIN" ] || { echo "rust-toolchain.toml has no exact channel" >&2; exit 1; }

run_cargo() {
	if command -v rustup >/dev/null 2>&1; then
		RUSTC=$(rustup which --toolchain "$TOOLCHAIN" rustc) \
		RUSTDOC=$(rustup which --toolchain "$TOOLCHAIN" rustdoc) \
			rustup run "$TOOLCHAIN" cargo "$@"
	else
		cargo "$@"
	fi
}

run_component() {
	component=$1
	shift
	if command -v rustup >/dev/null 2>&1 \
		&& rustup run "$TOOLCHAIN" "cargo-$component" --version >/dev/null 2>&1; then
		rustup run "$TOOLCHAIN" "cargo-$component" "$@"
	else
		cargo "$component" "$@"
	fi
}

test -f docs/TRANSCODE.md
test -f docs/COMPATIBILITY.md
test -f docs/DISTRIBUTION.md
test -x restart.sh
test ! -e improvement_plan.md
test ! -d testdata/cache
test ! -e testdata/testdata/cache/files.db
(bash -n restart.sh)
! RUSTY_DLNA_CACHE_VOLUME='invalid,volume' ./restart.sh >/dev/null 2>&1
! RUSTY_DLNA_START_TIMEOUT=0 ./restart.sh >/dev/null 2>&1
grep -F -q 'rusty-dlna-cache:/var/cache/rusty-dlna' docker-compose.yaml
grep -F -q 'docker compose create rusty-dlna' restart.sh
if command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
	docker compose config --quiet
	test "$(docker compose config --volumes)" = "${RUSTY_DLNA_CACHE_VOLUME:-rusty-dlna-cache}"
fi
(cd testdata && sha256sum --check SHA256SUMS)
! grep -Eqi 'disconnect kills|kill-on-disconnect|as a live fragmented-MP4 pipe|Transcoded GETs \(live pipe\)|OP=00 \(live pipe\)' \
	README.md docs/TRANSCODE.md
run_component fmt --all -- --check
run_component clippy --workspace --all-targets --all-features -- -D warnings
run_cargo test --workspace --locked
RUSTDOCFLAGS='-D warnings' run_cargo doc --workspace --no-deps --locked

CHECK_TMP=$(mktemp -d "${TMPDIR:-/tmp}/rusty-dlna-check.XXXXXX")
trap 'rm -rf "$CHECK_TMP"' EXIT INT TERM
printf 'friendly_name = "rustyDLNA check"\ncache_dir = "%s/cache"\ndb_dir = "%s/db"\nrescan_secs = 0\n' "$CHECK_TMP" "$CHECK_TMP" >"$CHECK_TMP/check.toml"
run_admin() {
	RUSTY_DLNA_HTTP_PORT=18220 RUSTY_DLNA_SSDP_PORT=11920 \
		run_cargo run --locked -p rusty-dlna -- --config "$CHECK_TMP/check.toml" "$@"
}
run_admin --check
run_admin --print-effective-config >"$CHECK_TMP/effective-config.txt"
grep -F -q 'http_port = 18220' "$CHECK_TMP/effective-config.txt"
run_admin --database-check
run_admin --rescan
run_admin --rebuild-database

# These tests fail if their explicitly requested ports are unavailable; they
# never silently turn the CI networking job green through an early return.
RUSTY_DLNA_REQUIRE_E2E=1 \
RUSTY_DLNA_HTTP_PORT=18200 \
RUSTY_DLNA_SSDP_PORT=11900 \
    run_cargo test --locked -p rusty-dlna --test listen_e2e -- --test-threads=1
# Host-side isolation (does not start rusty listeners)
if [ "${SKIP_ISOLATION:-}" != "1" ] && command -v curl >/dev/null 2>&1; then
	"$ROOT/scripts/assert-isolation.sh"
fi
if [ "${RUN_DOCKER_SMOKE:-0}" = "1" ]; then
	"$ROOT/scripts/compose-smoke.sh"
fi
echo "workspace OK"
