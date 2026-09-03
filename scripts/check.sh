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
		rustup run "$TOOLCHAIN" "cargo-$component" "$component" "$@"
	else
		cargo "$component" "$@"
	fi
}

test -f docs/TRANSCODE.md
test -f docs/COMPATIBILITY.md
test -f docs/DISTRIBUTION.md
test -f docs/OPERATIONS.md
test -f docs/LARGE_LIBRARY_BENCHMARK.md
test -x scripts/set-rust-version.sh
test -x scripts/cache-volume-init.sh
test -x scripts/cache-ownership-benchmark.sh
test -x scripts/large-library-benchmark.sh
test -x scripts/large-library-http-benchmark.py
test -x scripts/check-targeted-coverage.py
test -x scripts/promote-fuzz-regression.sh
test -x scripts/host-network-e2e.sh
test -x scripts/generate-advanced-fixtures.sh
test -x scripts/generate-dolby-vision-fixture.sh
test -x scripts/playwright-process-group.sh
test -x scripts/playwright-server.sh
test -x scripts/test-playwright-server.sh
test -x scripts/web-gateway-smoke.sh
test -x restart-web.sh
test -f Dockerfile.web
test -f docker-compose.web.yaml
test -f web-gateway/nginx.conf
test -f contrib/systemd/rusty-dlna.service
test -x contrib/library/update.sh
test -x contrib/library/maintain-library.py
test -x contrib/library/generate-dlna-previews.py
test -x contrib/library/clean-dead-links.sh
test -f contrib/library/README.md
test -f contrib/library/.env.example
test -f contrib/library/library.toml.example
test -f contrib/library/requirements.txt
! grep -Eq '^[[:space:]]*[^#[:space:]]' contrib/library/requirements.txt
! grep -Eq "export TMDB_API_(TOKEN|KEY)='[^']+'|export OMDB_API_KEYS='[^']+'" contrib/library/update.sh
python3 -c 'import ast, pathlib; root = pathlib.Path("contrib/library"); [ast.parse(path.read_text(encoding="utf-8")) for path in root.rglob("*.py") if "__pycache__" not in path.parts]'
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s contrib/library/tests -p 'test_*.py'
(bash -n contrib/library/update.sh contrib/library/clean-dead-links.sh contrib/library/fix-genre-permissions.sh contrib/library/lib/refresh-classification-data.sh contrib/library/lib/library-env.sh)
test -x restart.sh
grep -F -q "image: rust:$TOOLCHAIN-bookworm@sha256:" docker-compose.test.yaml
grep -F -q "test \"\$(rustc --version)\" = \"rustc $TOOLCHAIN " docker-compose.test.yaml
grep -F -q 'cargo test --workspace --locked' docker-compose.test.yaml
grep -F -q 'cargo run --locked -p rusty-dlna -- --check' docker-compose.test.yaml
(sh -n scripts/playwright-process-group.sh scripts/playwright-server.sh scripts/test-playwright-server.sh)
scripts/test-playwright-server.sh
if [ -d testdata/cache ]; then
	echo "error: testdata/cache exists; the quality gate requires a clean fixture tree" >&2
	echo "move this local runtime cache outside the repository, then rerun scripts/check.sh" >&2
	exit 1
fi
if [ -e testdata/testdata/cache/files.db ]; then
	echo "error: testdata/testdata/cache/files.db is unexpected generated state" >&2
	echo "move it outside the repository, then rerun scripts/check.sh" >&2
	exit 1
fi
if ! command -v node >/dev/null 2>&1; then
	echo "error: Node.js 20 or newer is required for the web unit tests" >&2
	exit 1
fi
if ! command -v npm >/dev/null 2>&1; then
	echo "error: npm is required for the web unit tests (install it with Node.js 20 or newer)" >&2
	exit 1
fi
NODE_MAJOR=$(node -p 'process.versions.node.split(".")[0]' 2>/dev/null) || {
	echo "error: could not determine the installed Node.js version" >&2
	exit 1
}
case "$NODE_MAJOR" in
	'' | *[!0-9]*)
		echo "error: could not determine the installed Node.js version" >&2
		exit 1
		;;
esac
if [ "$NODE_MAJOR" -lt 20 ]; then
	echo "error: Node.js 20 or newer is required for the web unit tests (found $(node --version))" >&2
	exit 1
fi
(bash -n restart.sh)
(bash -n restart-web.sh)
(sh -n scripts/web-gateway-smoke.sh)
(bash -n scripts/ssdp-netns-e2e.sh scripts/host-network-e2e.sh scripts/compose-smoke.sh scripts/large-library-benchmark.sh scripts/generate-advanced-fixtures.sh scripts/generate-dolby-vision-fixture.sh scripts/promote-fuzz-regression.sh)
python3 -c 'import ast, pathlib; [ast.parse(pathlib.Path(path).read_text(encoding="utf-8")) for path in ("scripts/large-library-http-benchmark.py", "scripts/check-targeted-coverage.py")]'
./restart.sh --help >/dev/null
./restart-web.sh --help >/dev/null
! ./restart.sh --nope >/dev/null 2>&1
! ./restart-web.sh --nope >/dev/null 2>&1
! RUSTY_DLNA_CACHE_VOLUME='invalid,volume' ./restart.sh >/dev/null 2>&1
! RUSTY_DLNA_START_TIMEOUT=0 ./restart.sh >/dev/null 2>&1
! RUSTY_DLNA_REPAIR_OWNERSHIP=invalid ./restart.sh >/dev/null 2>&1
! RUSTY_WEB_START_TIMEOUT=0 ./restart-web.sh >/dev/null 2>&1
grep -F -q 'rusty-dlna-cache-volume-init' restart.sh
! grep -F -q 'chown -R' restart.sh
grep -F -q 'rusty-dlna-cache:/var/cache/rusty-dlna' docker-compose.yaml
! grep -F -q ':/var/cache/rusty-dlna' docker-compose.override.yaml.example
grep -F -q 'docker compose create rusty-dlna' restart.sh
grep -F -q 'docker compose rm --force rusty-dlna' restart.sh
grep -F -q 'docker volume rm' restart.sh
grep -F -q -- '--clean' restart.sh
if command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
	docker compose config --quiet
	docker compose -f docker-compose.web.yaml config --quiet
	test -z "$(docker compose -f docker-compose.web.yaml config --volumes)"
	test "$(docker compose -f docker-compose.web.yaml config --services)" = "rusty-web"
	! docker compose -f docker-compose.web.yaml config | grep -Eq '(^|[[:space:]])1900([/:]|$)'
	test "$(docker compose config --volumes)" = "rusty-dlna-cache"
	docker compose config --format json | python3 -c \
		'import json, sys; name = json.load(sys.stdin)["volumes"]["rusty-dlna-cache"]["name"]; assert isinstance(name, str) and name.strip()'
	docker compose -f docker-compose.yaml -f docker-compose.override.yaml.example config --quiet
	test "$(docker compose -f docker-compose.yaml -f docker-compose.override.yaml.example config --volumes)" = \
		"rusty-dlna-cache"
	docker compose -f docker-compose.yaml -f docker-compose.override.yaml.example config --format json | python3 -c \
		'import json, sys; name = json.load(sys.stdin)["volumes"]["rusty-dlna-cache"]["name"]; assert isinstance(name, str) and name.strip()'
fi
if command -v systemd-analyze >/dev/null 2>&1; then
	if ! SYSTEMD_VERIFY=$(systemd-analyze verify contrib/systemd/rusty-dlna.service 2>&1); then
		# The example's native binary is intentionally not installed on CI hosts.
		SYSTEMD_UNEXPECTED=$(printf '%s\n' "$SYSTEMD_VERIFY" | \
			grep -Fv 'Command /usr/local/bin/rusty-dlna is not executable: No such file or directory' || true)
		test -n "$SYSTEMD_VERIFY" && test -z "$SYSTEMD_UNEXPECTED" || {
			printf '%s\n' "$SYSTEMD_VERIFY" >&2
			exit 1
		}
	fi
fi
(cd testdata && sha256sum --check SHA256SUMS)
REQUIRED_FIXTURES=$(sed '/^[[:space:]]*$/d' testdata/REQUIRED_FILES | LC_ALL=C sort)
ACTUAL_FIXTURES=$(find testdata/library -type f -printf '%P\n' | LC_ALL=C sort)
test "$ACTUAL_FIXTURES" = "$REQUIRED_FIXTURES" || {
	echo "testdata/library differs from testdata/REQUIRED_FILES" >&2
	printf 'required:\n%s\nactual:\n%s\n' "$REQUIRED_FIXTURES" "$ACTUAL_FIXTURES" >&2
	exit 1
}
test "$(file -b --mime-encoding scripts/fixture-exif.rs)" != binary
FIXTURE_STATE_BEFORE=$(find testdata/library -type f -exec sha256sum {} + | LC_ALL=C sort)
! grep -Eqi 'disconnect kills|kill-on-disconnect|as a live fragmented-MP4 pipe|Transcoded GETs \(live pipe\)|OP=00 \(live pipe\)' \
	README.md docs/TRANSCODE.md
npm run test:web-unit
run_component fmt --all -- --check
run_component clippy --workspace --all-targets --all-features -- -D warnings
run_cargo test --workspace --locked
FIXTURE_STATE_AFTER=$(find testdata/library -type f -exec sha256sum {} + | LC_ALL=C sort)
test "$FIXTURE_STATE_AFTER" = "$FIXTURE_STATE_BEFORE" || {
	echo "tests modified tracked fixture inputs under testdata/library" >&2
	exit 1
}
RUSTDOCFLAGS='-D warnings' run_cargo doc --workspace --no-deps --locked

CHECK_TMP=$(mktemp -d "${TMPDIR:-/tmp}/rusty-dlna-check.XXXXXX")
cleanup_check_tmp() {
	status=$?
	trap - EXIT
	trap '' INT TERM HUP
	rm -rf -- "$CHECK_TMP"
	exit "$status"
}
trap cleanup_check_tmp EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
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
