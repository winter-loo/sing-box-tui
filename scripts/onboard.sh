#!/usr/bin/env sh
set -eu

VERSION=
FORCE=0
CHECK_ONLY=0
DRY_RUN=0
INSTALL_URL=https://sing-box.app/install.sh

usage() {
  cat <<'EOF'
Usage: scripts/onboard.sh [--version VERSION] [--force] [--check-only] [--dry-run]

Installs sing-box with:
  curl -fsSL https://sing-box.app/install.sh | sh

For a specific version:
  curl -fsSL https://sing-box.app/install.sh | sh -s -- --version <version>

Skips installation when sing-box is already on PATH unless --force is set.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --help|-h)
      usage
      exit 0
      ;;
    --version)
      if [ "$#" -lt 2 ]; then
        echo "error: --version requires a value" >&2
        exit 2
      fi
      VERSION=$2
      shift 2
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

if command -v sing-box >/dev/null 2>&1 && [ "$FORCE" -eq 0 ]; then
  echo "sing-box already found: $(command -v sing-box)"
  exit 0
fi

if [ "$CHECK_ONLY" -eq 1 ]; then
  echo "sing-box not found on PATH"
  exit 1
fi

if ! command -v curl >/dev/null 2>&1; then
  echo "error: curl was not found" >&2
  exit 1
fi

if [ -n "$VERSION" ]; then
  if [ "$DRY_RUN" -eq 1 ]; then
    echo "curl -fsSL $INSTALL_URL | sh -s -- --version $VERSION"
    exit 0
  fi
  curl -fsSL "$INSTALL_URL" | sh -s -- --version "$VERSION"
else
  if [ "$DRY_RUN" -eq 1 ]; then
    echo "curl -fsSL $INSTALL_URL | sh"
    exit 0
  fi
  curl -fsSL "$INSTALL_URL" | sh
fi
