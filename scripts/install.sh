#!/usr/bin/env sh
set -eu

REPO=${REPO:-winter-loo/sing-box-tui}
VERSION=${VERSION:-latest}
INSTALL_DIR=${INSTALL_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/sing-box-tui}
SING_BOX_REPO=${SING_BOX_REPO:-winter-loo/sing-box}
SING_BOX_VERSION=${SING_BOX_VERSION:-v1.13.13-winterloo.2}
SING_BOX_SHA256=${SING_BOX_SHA256:-auto}
GITHUB_PROXY=${GITHUB_PROXY-https://deeloo.cn/anywhere}
DOWNLOAD_PARTS=${DOWNLOAD_PARTS:-4}
DOWNLOAD_TIMEOUT_SEC=${DOWNLOAD_TIMEOUT_SEC:-600}
DOWNLOAD_STALL_TIMEOUT_SEC=${DOWNLOAD_STALL_TIMEOUT_SEC:-30}
SKIP_SING_BOX=0
NO_PATH=0
FORCE=0
CHECK_ONLY=0
DRY_RUN=0
USER_AGENT=sing-box-tui-installer
TEMP_DIR=

usage() {
  cat <<'EOF'
Usage: scripts/install.sh [OPTIONS]

Install the sing-box-tui release for the current Unix platform and install the
configured sing-box core release when sing-box is not already on PATH.

Options:
  --repo OWNER/REPO              sing-box-tui GitHub repository
  --version VERSION              sing-box-tui release tag (default: latest)
  --install-dir DIR              installation directory
  --sing-box-repo OWNER/REPO     sing-box GitHub repository
  --sing-box-version VERSION     sing-box release tag
  --sing-box-sha256 SHA256       expected core hash, or "auto" for asset digest
  --github-proxy URL             fallback prefix for GitHub requests; "" disables
  --download-parts COUNT         parallel download parts, 1-16 (default: 4)
  --download-timeout-sec SEC     total request timeout, 1-3600 (default: 600)
  --download-stall-timeout-sec SEC
                                 no-progress timeout, 1-600 (default: 30)
  --skip-sing-box                do not install the sing-box core
  --add-to-path                  add the install directory to the user PATH
  --no-path                      do not add the TUI install directory to PATH
  --force                        replace files already in the install directory
  --check-only                   check whether the requested tools are installed
  --dry-run                      print the planned installation without changing it
  -h, --help                     show this help

Defaults can also be overridden with the corresponding uppercase environment
variables, for example REPO, INSTALL_DIR, GITHUB_PROXY, and DOWNLOAD_PARTS.
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

write_step() {
  echo "==> $*"
}

require_value() {
  if [ "$#" -lt 2 ]; then
    echo "error: $1 requires a value" >&2
    exit 2
  fi
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --help|-h)
      usage
      exit 0
      ;;
    --repo)
      require_value "$@"
      REPO=$2
      shift 2
      ;;
    --version)
      require_value "$@"
      VERSION=$2
      shift 2
      ;;
    --install-dir)
      require_value "$@"
      INSTALL_DIR=$2
      shift 2
      ;;
    --sing-box-repo)
      require_value "$@"
      SING_BOX_REPO=$2
      shift 2
      ;;
    --sing-box-version)
      require_value "$@"
      SING_BOX_VERSION=$2
      shift 2
      ;;
    --sing-box-sha256)
      require_value "$@"
      SING_BOX_SHA256=$2
      shift 2
      ;;
    --github-proxy)
      require_value "$@"
      GITHUB_PROXY=$2
      shift 2
      ;;
    --download-parts)
      require_value "$@"
      DOWNLOAD_PARTS=$2
      shift 2
      ;;
    --download-timeout-sec)
      require_value "$@"
      DOWNLOAD_TIMEOUT_SEC=$2
      shift 2
      ;;
    --download-stall-timeout-sec)
      require_value "$@"
      DOWNLOAD_STALL_TIMEOUT_SEC=$2
      shift 2
      ;;
    --skip-sing-box)
      SKIP_SING_BOX=1
      shift
      ;;
    --add-to-path)
      NO_PATH=0
      shift
      ;;
    --no-path)
      NO_PATH=1
      shift
      ;;
    --force)
      FORCE=1
      shift
      ;;
    --check-only)
      CHECK_ONLY=1
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

validate_range() {
  value=$1
  minimum=$2
  maximum=$3
  option=$4
  case "$value" in
    ''|*[!0-9]*)
      echo "error: $option must be an integer from $minimum to $maximum" >&2
      exit 2
      ;;
  esac
  if [ "$value" -lt "$minimum" ] || [ "$value" -gt "$maximum" ]; then
    echo "error: $option must be an integer from $minimum to $maximum" >&2
    exit 2
  fi
}

validate_range "$DOWNLOAD_PARTS" 1 16 --download-parts
validate_range "$DOWNLOAD_TIMEOUT_SEC" 1 3600 --download-timeout-sec
validate_range "$DOWNLOAD_STALL_TIMEOUT_SEC" 1 600 --download-stall-timeout-sec

case $(uname -s) in
  Linux)
    PLATFORM_NAME=Linux
    TARGET_OS=linux
    ;;
  Darwin)
    PLATFORM_NAME=macOS
    TARGET_OS=darwin
    ;;
  *)
    die "unsupported operating system: $(uname -s)"
    ;;
esac

case $(uname -m) in
  x86_64|amd64)
    TARGET_ARCH=x86_64
    CORE_ARCH=amd64
    ;;
  arm64|aarch64)
    TARGET_ARCH=aarch64
    CORE_ARCH=arm64
    ;;
  *)
    die "unsupported architecture: $(uname -m)"
    ;;
esac

case "$TARGET_OS/$TARGET_ARCH" in
  linux/x86_64)
    TARGET_TRIPLE=x86_64-unknown-linux-gnu
    ;;
  darwin/x86_64)
    TARGET_TRIPLE=x86_64-apple-darwin
    ;;
  darwin/aarch64)
    TARGET_TRIPLE=aarch64-apple-darwin
    ;;
  *)
    die "no sing-box-tui release is published for $PLATFORM_NAME $(uname -m)"
    ;;
esac

TUI_EXE=$INSTALL_DIR/sing-box-tui
CORE_DIR=$INSTALL_DIR/core
CORE_EXE=$CORE_DIR/sing-box

check_installation() {
  status=0
  if command -v sing-box-tui >/dev/null 2>&1; then
    echo "sing-box-tui found: $(command -v sing-box-tui)"
  elif [ -f "$TUI_EXE" ] && [ -x "$TUI_EXE" ]; then
    echo "sing-box-tui found: $TUI_EXE"
  else
    echo "sing-box-tui not found"
    status=1
  fi

  if [ "$SKIP_SING_BOX" -eq 0 ]; then
    if command -v sing-box >/dev/null 2>&1; then
      echo "sing-box found: $(command -v sing-box)"
    elif [ -f "$CORE_EXE" ] && [ -x "$CORE_EXE" ]; then
      echo "sing-box found: $CORE_EXE"
    else
      echo "sing-box not found"
      status=1
    fi
  fi
  return "$status"
}

if [ "$CHECK_ONLY" -eq 1 ]; then
  if check_installation; then
    exit 0
  else
    exit 1
  fi
fi

if [ "$DRY_RUN" -eq 1 ]; then
  write_step "Platform: $PLATFORM_NAME $(uname -m) ($TARGET_TRIPLE)"
  if [ "$SKIP_SING_BOX" -eq 0 ]; then
    if command -v sing-box >/dev/null 2>&1; then
      write_step "sing-box already found: $(command -v sing-box)"
    elif [ -f "$CORE_EXE" ] && [ "$FORCE" -eq 0 ]; then
      if [ -x "$CORE_EXE" ]; then
        write_step "sing-box core already installed at $CORE_EXE"
      else
        write_step "Would restore execute permission on $CORE_EXE"
      fi
    else
      core_version=${SING_BOX_VERSION#v}
      core_asset=sing-box-$core_version-$TARGET_OS-$CORE_ARCH
      write_step "Would install $SING_BOX_REPO $SING_BOX_VERSION asset $core_asset to $CORE_EXE"
    fi
  fi
  if [ -f "$TUI_EXE" ] && [ "$FORCE" -eq 0 ]; then
    if [ -x "$TUI_EXE" ]; then
      write_step "sing-box-tui already installed at $TUI_EXE"
    else
      write_step "Would restore execute permission on $TUI_EXE"
    fi
  else
    tui_version=$VERSION
    if [ "$VERSION" = latest ]; then
      tui_version='<latest-tag>'
    fi
    tui_asset=sing-box-tui-$tui_version-$TARGET_TRIPLE.tar.gz
    write_step "Would install $REPO $VERSION asset $tui_asset to $TUI_EXE"
  fi
  if [ "$NO_PATH" -eq 0 ]; then
    write_step "Would add $INSTALL_DIR to the user PATH"
  fi
  exit 0
fi

command -v curl >/dev/null 2>&1 || die "curl was not found"
command -v awk >/dev/null 2>&1 || die "awk was not found"
command -v tar >/dev/null 2>&1 || die "tar was not found"

TEMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/sing-box-tui-install.XXXXXX") ||
  die "could not create a temporary directory"

cleanup() {
  if [ -n "$TEMP_DIR" ] && [ -d "$TEMP_DIR" ]; then
    rm -rf "$TEMP_DIR"
  fi
}
trap cleanup 0 HUP INT TERM

proxy_url() {
  echo "${GITHUB_PROXY%/}/$1"
}

curl_request() {
  url=$1
  output=$2
  accept=${3:-application/json}
  curl -fLsS \
    --retry 2 \
    --retry-delay 1 \
    --connect-timeout "$DOWNLOAD_STALL_TIMEOUT_SEC" \
    --max-time "$DOWNLOAD_TIMEOUT_SEC" \
    --speed-limit 1 \
    --speed-time "$DOWNLOAD_STALL_TIMEOUT_SEC" \
    -A "$USER_AGENT" \
    -H "Accept: $accept" \
    -o "$output" \
    "$url"
}

github_api() {
  url=$1
  output=$2
  if curl_request "$url" "$output" application/vnd.github+json 2>/dev/null; then
    return 0
  fi
  rm -f "$output"
  if [ -z "$GITHUB_PROXY" ]; then
    die "GitHub API request failed: $url"
  fi
  write_step "GitHub is not directly accessible; retrying through $GITHUB_PROXY"
  curl_request "$(proxy_url "$url")" "$output" application/vnd.github+json ||
    die "GitHub API request failed through $GITHUB_PROXY: $url"
}

get_release() {
  repo=$1
  version=$2
  output=$3
  if [ "$version" = latest ]; then
    release_url=https://api.github.com/repos/$repo/releases/latest
  else
    release_url=https://api.github.com/repos/$repo/releases/tags/$version
  fi
  github_api "$release_url" "$output"
}

json_release_tag() {
  sed -e 's/},{/\
{/g' -e 's/,"/\
"/g' -e 's/\[{/\
{/g' "$1" | awk '
    /^[[:space:]+{]*"tag_name"[[:space:]]*:/ {
      value = $0
      sub(/^[^:]*:[[:space:]]*"/, "", value)
      sub(/".*$/, "", value)
      print value
      exit
    }
  '
}

json_release_asset() {
  sed -e 's/},{/\
{/g' -e 's/,"/\
"/g' -e 's/\[{/\
{/g' "$1" | awk -v expected="$2" '
    function json_value(line) {
      sub(/^[^:]*:[[:space:]]*"/, "", line)
      sub(/".*$/, "", line)
      gsub(/\\\//, "/", line)
      return line
    }
    /^[[:space:]+{]*"url"[[:space:]]*:/ {
      candidate = json_value($0)
      if (candidate ~ /\/releases\/assets\//) {
        api_url = candidate
      }
    }
    /^[[:space:]+{]*"name"[[:space:]]*:/ { name = json_value($0) }
    /^[[:space:]+{]*"digest"[[:space:]]*:/ { digest = json_value($0) }
    /^[[:space:]+{]*"browser_download_url"[[:space:]]*:/ {
      browser_url = json_value($0)
      if (name == expected) {
        print api_url "|" browser_url "|" digest
        exit
      }
      api_url = ""
      name = ""
      digest = ""
    }
  '
}

curl_download() {
  url=$1
  output=$2
  accept=$3
  if [ -t 2 ]; then
    curl -fL --progress-bar \
      --retry 2 \
      --retry-delay 1 \
      --connect-timeout "$DOWNLOAD_STALL_TIMEOUT_SEC" \
      --max-time "$DOWNLOAD_TIMEOUT_SEC" \
      --speed-limit 1 \
      --speed-time "$DOWNLOAD_STALL_TIMEOUT_SEC" \
      -A "$USER_AGENT" \
      -H "Accept: $accept" \
      -o "$output" \
      "$url"
  else
    curl_request "$url" "$output" "$accept"
  fi
}

parallel_download() {
  url=$1
  output=$2
  accept=$3
  parts=$4
  headers=$TEMP_DIR/download-headers

  if ! curl -fLsSI \
    --connect-timeout "$DOWNLOAD_STALL_TIMEOUT_SEC" \
    --max-time "$DOWNLOAD_TIMEOUT_SEC" \
    -A "$USER_AGENT" \
    -H "Accept: $accept" \
    -o "$headers" \
    "$url"; then
    return 1
  fi

  length=$(awk '
    tolower($1) == "content-length:" {
      gsub(/\r/, "", $2)
      content_length = $2
    }
    END { print content_length }
  ' "$headers")
  accept_ranges=$(awk '
    tolower($1) == "accept-ranges:" {
      gsub(/\r/, "", $2)
      ranges = tolower($2)
    }
    END { print ranges }
  ' "$headers")

  case "$length" in
    ''|*[!0-9]*) return 1 ;;
  esac
  if [ "$length" -le 0 ]; then
    return 1
  fi
  if [ -n "$accept_ranges" ] && [ "$accept_ranges" != bytes ]; then
    return 1
  fi
  if [ "$length" -gt 2147483647 ]; then
    return 1
  fi
  if [ "$parts" -gt "$length" ]; then
    parts=$length
  fi

  chunk_size=$(( (length + parts - 1) / parts ))
  index=0
  pids=
  while [ "$index" -lt "$parts" ]; do
    start=$(( index * chunk_size ))
    end=$(( start + chunk_size - 1 ))
    if [ "$end" -ge "$length" ]; then
      end=$(( length - 1 ))
    fi
    part_file=$output.part.$index
    rm -f "$part_file"
    curl -fLsS \
      --retry 2 \
      --retry-delay 1 \
      --connect-timeout "$DOWNLOAD_STALL_TIMEOUT_SEC" \
      --max-time "$DOWNLOAD_TIMEOUT_SEC" \
      --speed-limit 1 \
      --speed-time "$DOWNLOAD_STALL_TIMEOUT_SEC" \
      -A "$USER_AGENT" \
      -H "Accept: $accept" \
      --range "$start-$end" \
      -o "$part_file" \
      "$url" &
    pids="$pids $!"
    index=$(( index + 1 ))
  done

  failed=0
  for pid in $pids; do
    if ! wait "$pid"; then
      failed=1
    fi
  done

  index=0
  while [ "$index" -lt "$parts" ]; do
    start=$(( index * chunk_size ))
    end=$(( start + chunk_size - 1 ))
    if [ "$end" -ge "$length" ]; then
      end=$(( length - 1 ))
    fi
    expected=$(( end - start + 1 ))
    part_file=$output.part.$index
    if [ ! -f "$part_file" ]; then
      failed=1
    else
      actual=$(wc -c < "$part_file" | tr -d '[:space:]')
      if [ "$actual" != "$expected" ]; then
        failed=1
      fi
    fi
    index=$(( index + 1 ))
  done

  if [ "$failed" -ne 0 ]; then
    index=0
    while [ "$index" -lt "$parts" ]; do
      rm -f "$output.part.$index"
      index=$(( index + 1 ))
    done
    return 1
  fi

  : > "$output"
  index=0
  while [ "$index" -lt "$parts" ]; do
    part_file=$output.part.$index
    cat "$part_file" >> "$output"
    rm -f "$part_file"
    index=$(( index + 1 ))
  done
  actual=$(wc -c < "$output" | tr -d '[:space:]')
  [ "$actual" = "$length" ]
}

download_url() {
  url=$1
  output=$2
  accept=$3
  if [ "$DOWNLOAD_PARTS" -gt 1 ]; then
    write_step "Downloading with $DOWNLOAD_PARTS parallel parts: $url"
    if parallel_download "$url" "$output" "$accept" "$DOWNLOAD_PARTS"; then
      return 0
    fi
    rm -f "$output"
    write_step "Parallel download unavailable; falling back to a single request"
  fi
  write_step "Downloading with a single request: $url"
  curl_download "$url" "$output" "$accept"
}

download_github_asset() {
  record=$1
  output=$2
  api_url=${record%%|*}
  remainder=${record#*|}
  browser_url=${remainder%%|*}
  if download_url "$browser_url" "$output" application/octet-stream; then
    return 0
  fi
  rm -f "$output"
  if [ -z "$GITHUB_PROXY" ]; then
    die "GitHub asset download failed: $browser_url"
  fi
  write_step "Direct GitHub download failed; retrying through $GITHUB_PROXY"
  download_url "$(proxy_url "$api_url")" "$output" application/octet-stream ||
    die "GitHub asset download failed through $GITHUB_PROXY: $browser_url"
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{ print $1 }'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$1" | awk '{ print $NF }'
  else
    die "sha256sum, shasum, or openssl is required to verify the sing-box core"
  fi
}

append_profile_line() {
  profile=$1
  path_line=$2
  profile_dir=${profile%/*}
  if [ "$profile_dir" != "$profile" ]; then
    if [ -z "$profile_dir" ]; then
      profile_dir=/
    fi
    mkdir -p "$profile_dir"
  fi
  if [ ! -f "$profile" ] || ! grep -Fqx "$path_line" "$profile"; then
    {
      echo
      echo '# Added by the sing-box-tui installer'
      echo "$path_line"
    } >> "$profile"
  fi
}

add_user_path() {
  path_to_add=$1
  escaped_path=$(printf '%s' "$path_to_add" | sed "s/'/'\\\\''/g")
  posix_path_line="case \"\$PATH\" in *'$escaped_path'*) ;; *) export PATH=\"\$PATH\":'$escaped_path' ;; esac"

  if [ -n "${SING_BOX_TUI_PROFILE-}" ]; then
    case "$SING_BOX_TUI_PROFILE" in
      *.fish)
        fish_path=$(printf '%s' "$path_to_add" | sed "s/'/\\\\'/g")
        append_profile_line "$SING_BOX_TUI_PROFILE" "fish_add_path --append '$fish_path'"
        ;;
      *)
        append_profile_line "$SING_BOX_TUI_PROFILE" "$posix_path_line"
        ;;
    esac
  else
    shell_name=${SHELL-}
    shell_name=${shell_name##*/}
    case "$shell_name" in
      bash)
        append_profile_line "$HOME/.bashrc" "$posix_path_line"
        if [ -f "$HOME/.bash_profile" ]; then
          login_profile=$HOME/.bash_profile
        elif [ -f "$HOME/.bash_login" ]; then
          login_profile=$HOME/.bash_login
        else
          login_profile=$HOME/.profile
        fi
        append_profile_line "$login_profile" "$posix_path_line"
        ;;
      zsh)
        zsh_config_dir=${ZDOTDIR:-$HOME}
        append_profile_line "$zsh_config_dir/.zshenv" "$posix_path_line"
        ;;
      fish)
        fish_config_dir=${XDG_CONFIG_HOME:-$HOME/.config}/fish
        fish_path=$(printf '%s' "$path_to_add" | sed "s/'/\\\\'/g")
        append_profile_line "$fish_config_dir/config.fish" "fish_add_path --append '$fish_path'"
        ;;
      *)
        append_profile_line "$HOME/.profile" "$posix_path_line"
        ;;
    esac
  fi

  case ":$PATH:" in
    *:"$path_to_add":*) ;;
    *) PATH=$PATH:$path_to_add ;;
  esac
  export PATH
}

install_sing_box_core() {
  if command -v sing-box >/dev/null 2>&1; then
    write_step "sing-box already found: $(command -v sing-box)"
    return 0
  fi
  if [ -f "$CORE_EXE" ] && [ "$FORCE" -eq 0 ]; then
    if [ ! -x "$CORE_EXE" ]; then
      chmod u+x "$CORE_EXE"
      write_step "Restored execute permission on $CORE_EXE"
    fi
    write_step "sing-box core already installed at $CORE_EXE"
    add_user_path "$CORE_DIR"
    return 0
  fi

  release_json=$TEMP_DIR/sing-box-release.json
  get_release "$SING_BOX_REPO" "$SING_BOX_VERSION" "$release_json"
  core_version=${SING_BOX_VERSION#v}
  core_asset=sing-box-$core_version-$TARGET_OS-$CORE_ARCH
  asset=$(json_release_asset "$release_json" "$core_asset")
  if [ -z "$asset" ]; then
    die "no $PLATFORM_NAME $CORE_ARCH sing-box binary asset found in $SING_BOX_REPO release '$SING_BOX_VERSION'"
  fi

  download=$TEMP_DIR/$core_asset
  write_step "Downloading sing-box core $core_asset"
  download_github_asset "$asset" "$download"

  remainder=${asset#*|}
  digest=${remainder#*|}
  expected_sha=$SING_BOX_SHA256
  if [ "$expected_sha" = auto ]; then
    case "$digest" in
      sha256:*) expected_sha=${digest#sha256:} ;;
      *) die "GitHub did not provide a SHA256 digest for $core_asset; pass --sing-box-sha256" ;;
    esac
  fi
  if [ -n "$expected_sha" ]; then
    expected_sha=$(printf '%s' "$expected_sha" | tr '[:upper:]' '[:lower:]')
    actual_sha=$(sha256_file "$download" | tr '[:upper:]' '[:lower:]')
    if [ "$actual_sha" != "$expected_sha" ]; then
      die "sing-box SHA256 mismatch. Expected $expected_sha but got $actual_sha"
    fi
  fi

  mkdir -p "$CORE_DIR"
  cp "$download" "$CORE_EXE"
  chmod 755 "$CORE_EXE"
  add_user_path "$CORE_DIR"
  write_step "Installed sing-box core to $CORE_EXE"
}

install_tui() {
  mkdir -p "$INSTALL_DIR"
  if [ -f "$TUI_EXE" ] && [ "$FORCE" -eq 0 ]; then
    if [ ! -x "$TUI_EXE" ]; then
      chmod u+x "$TUI_EXE"
      write_step "Restored execute permission on $TUI_EXE"
    fi
    write_step "sing-box-tui already installed at $TUI_EXE"
    return 0
  fi

  release_json=$TEMP_DIR/sing-box-tui-release.json
  get_release "$REPO" "$VERSION" "$release_json"
  release_tag=$(json_release_tag "$release_json")
  if [ -z "$release_tag" ]; then
    die "could not resolve the release tag for $REPO '$VERSION'"
  fi
  tui_asset=sing-box-tui-$release_tag-$TARGET_TRIPLE.tar.gz
  asset=$(json_release_asset "$release_json" "$tui_asset")
  if [ -z "$asset" ]; then
    die "no $PLATFORM_NAME $TARGET_ARCH tar.gz asset found in $REPO release '$release_tag'"
  fi

  archive=$TEMP_DIR/$tui_asset
  extract=$TEMP_DIR/extract
  write_step "Downloading $tui_asset"
  download_github_asset "$asset" "$archive"
  mkdir -p "$extract"
  tar -xzf "$archive" -C "$extract"
  downloaded_exe=$(find "$extract" -type f -name sing-box-tui -print | sed -n '1p')
  if [ -z "$downloaded_exe" ]; then
    die "downloaded archive did not contain sing-box-tui"
  fi
  cp "$downloaded_exe" "$TUI_EXE"
  chmod 755 "$TUI_EXE"
  write_step "Installed sing-box-tui to $TUI_EXE"
}

if [ "$SKIP_SING_BOX" -eq 0 ]; then
  install_sing_box_core
fi

install_tui

if [ "$NO_PATH" -eq 0 ]; then
  add_user_path "$INSTALL_DIR"
  write_step "Added $INSTALL_DIR to the user PATH"
fi

echo
echo 'Run:'
echo "  \"$TUI_EXE\" run"
