#!/usr/bin/env sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/sing-box-tui-installer-test.XXXXXX")
trap 'rm -rf "$TEST_ROOT"' 0 HUP INT TERM

case $(uname -s) in
  Darwin) target_os=darwin ;;
  Linux) target_os=linux ;;
  *) echo "skip: unsupported test platform"; exit 0 ;;
esac
case $(uname -m) in
  x86_64|amd64) target_arch=x86_64 ;;
  arm64|aarch64) target_arch=aarch64 ;;
  *) echo "skip: unsupported test architecture"; exit 0 ;;
esac
case "$target_os/$target_arch" in
  linux/x86_64) target_triple=x86_64-unknown-linux-gnu ;;
  darwin/x86_64) target_triple=x86_64-apple-darwin ;;
  darwin/aarch64) target_triple=aarch64-apple-darwin ;;
  *) echo "skip: no release target for test platform"; exit 0 ;;
esac

fixture=$TEST_ROOT/fixture
mkdir -p "$fixture/package" "$TEST_ROOT/bin"
printf '#!/usr/bin/env sh\necho test-version\n' > "$fixture/package/sing-box-tui"
chmod 755 "$fixture/package/sing-box-tui"
tar -czf "$fixture/asset.tar.gz" -C "$fixture/package" sing-box-tui
asset_name=sing-box-tui-v-test-$target_triple.tar.gz
cat > "$fixture/release.json" <<EOF
{"tag_name":"v-test","assets":[{"url":"https://api.test/releases/assets/1","name":"$asset_name","digest":"sha256:test","browser_download_url":"https://direct.test/$asset_name"}]}
EOF

cat > "$TEST_ROOT/bin/curl" <<'EOF'
#!/usr/bin/env sh
set -eu
output=
headers=
write_out=
range=
fail_on_http_error=0
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o|-D|-w|--range|-A|-H|--retry|--retry-delay|--connect-timeout|--max-time|--speed-limit|--speed-time)
      option=$1
      value=$2
      shift 2
      case "$option" in
        -o) output=$value ;;
        -D) headers=$value ;;
        -w) write_out=$value ;;
        --range) range=$value ;;
      esac
      ;;
    -*)
      case "$1" in *f*) fail_on_http_error=1 ;; esac
      shift
      ;;
    *) url=$1; shift ;;
  esac
done

case "$url" in
  https://api.github.com/repos/*)
    cp "$TEST_FIXTURE/release.json" "$output"
    exit 0
    ;;
esac

route=direct
case "$url" in *https://api.test/releases/assets/1*) route=proxy ;; esac
printf '%s %s\n' "$route" "${range:-full}" >> "$TEST_LOG"

if [ "$TEST_SCENARIO" = direct-failure ] && [ "$route" = direct ]; then
  exit 28
fi

size=$(wc -c < "$TEST_FIXTURE/asset.tar.gz" | tr -d '[:space:]')
status=206
content_range=
if [ -z "$range" ]; then
  status=200
  cp "$TEST_FIXTURE/asset.tar.gz" "$output"
elif [ "$TEST_SCENARIO" = ignored-range ]; then
  status=200
  cp "$TEST_FIXTURE/asset.tar.gz" "$output"
elif [ "$TEST_SCENARIO" = rejected-range ]; then
  status=416
  : > "$output"
else
  start=${range%-*}
  end=${range#*-}
  count=$((end - start + 1))
  if [ "$TEST_SCENARIO" = transient-part ] && [ "$start" -gt 0 ]; then
    marker=$TEST_STATE/transient-part-failed
    if [ ! -f "$marker" ]; then
      : > "$marker"
      exit 28
    fi
  fi
  dd if="$TEST_FIXTURE/asset.tar.gz" of="$output" bs=1 skip="$start" count="$count" 2>/dev/null
  content_range="bytes $start-$end/$size"
  if [ "$TEST_SCENARIO" = wrong-range ] && [ "$route" = direct ]; then
    content_range="bytes 0-0/$size"
  fi
fi

if [ -n "$headers" ]; then
  {
    printf 'HTTP/1.1 %s Test\r\n' "$status"
    [ -z "$content_range" ] || printf 'Content-Range: %s\r\n' "$content_range"
    printf '\r\n'
  } > "$headers"
fi
[ -z "$write_out" ] || printf '%s' "$status"
if [ "$fail_on_http_error" -eq 1 ] && [ "$status" -ge 400 ]; then
  exit 22
fi
EOF
chmod 755 "$TEST_ROOT/bin/curl"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

run_case() {
  scenario=$1
  case_dir=$TEST_ROOT/$scenario
  mkdir -p "$case_dir/state"
  : > "$case_dir/curl.log"
  TEST_FIXTURE=$fixture \
  TEST_SCENARIO=$scenario \
  TEST_STATE=$case_dir/state \
  TEST_LOG=$case_dir/curl.log \
  PATH="$TEST_ROOT/bin:$PATH" \
    sh "$ROOT/scripts/install.sh" \
      --install-dir "$case_dir/install" \
      --skip-sing-box \
      --no-path \
      --force \
      --no-macos-helper \
      --github-proxy https://proxy.test/anywhere >/dev/null
  [ -x "$case_dir/install/sing-box-tui" ] || fail "$scenario did not install an executable"
  [ "$("$case_dir/install/sing-box-tui")" = test-version ] || fail "$scenario installed the wrong payload"
  if find "$case_dir" -name '*.part.*' -print | grep . >/dev/null; then
    fail "$scenario left download parts behind"
  fi
}

run_case valid-range
run_case ignored-range
[ "$(grep -c '^direct ' "$TEST_ROOT/ignored-range/curl.log")" -eq 1 ] ||
  fail "ignored range downloaded the direct asset more than once"

run_case rejected-range
grep -q '^direct full$' "$TEST_ROOT/rejected-range/curl.log" ||
  fail "explicit range rejection did not use a direct single request"

run_case transient-part
retry_count=$(sort "$TEST_ROOT/transient-part/curl.log" | uniq -c | awk '$1 == 2 && $3 != "0-0" { count++ } END { print count + 0 }')
[ "$retry_count" -ge 1 ] || fail "transient part was not retried"

run_case wrong-range
grep -q '^proxy ' "$TEST_ROOT/wrong-range/curl.log" ||
  fail "malformed direct ranges did not advance to the proxy"

run_case direct-failure
grep -q '^proxy ' "$TEST_ROOT/direct-failure/curl.log" ||
  fail "direct transport failure did not advance to the proxy"
if grep -q '^direct full$' "$TEST_ROOT/direct-failure/curl.log"; then
  fail "direct transport failure retried as a full direct download"
fi

printf 'installer download tests passed\n'
