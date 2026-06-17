#!/usr/bin/env sh
set -eu

VERSION=
FORCE=0
CHECK_ONLY=0
DRY_RUN=0

usage() {
  cat <<'EOF'
Usage: scripts/install-python.sh [--version VERSION] [--force] [--check-only] [--dry-run]

Installs Python 3 with the platform package manager.
On macOS, Homebrew is used.
On Linux, apt, dnf, yum, pacman, zypper, or apk is used when available.
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

if command -v python3 >/dev/null 2>&1 && [ "$FORCE" -eq 0 ]; then
  echo "Python 3 already found: $(command -v python3)"
  exit 0
fi

if [ "$CHECK_ONLY" -eq 1 ]; then
  echo "Python 3 not found"
  exit 1
fi

run_cmd() {
  if [ "$DRY_RUN" -eq 1 ]; then
    printf '%s\n' "$*"
  else
    "$@"
  fi
}

if [ "$(uname -s)" = "Darwin" ]; then
  if ! command -v brew >/dev/null 2>&1; then
    echo "error: Homebrew was not found. Install Homebrew, then rerun this script." >&2
    exit 1
  fi
  if [ -n "$VERSION" ]; then
    run_cmd brew install "python@$VERSION"
  else
    run_cmd brew install python
  fi
  exit 0
fi

if command -v apt-get >/dev/null 2>&1; then
  if [ "$DRY_RUN" -eq 1 ]; then
    echo "sudo apt-get update"
    if [ -n "$VERSION" ]; then
      echo "sudo apt-get install -y python$VERSION python$VERSION-venv"
    else
      echo "sudo apt-get install -y python3 python3-pip python3-venv"
    fi
    exit 0
  fi
  sudo apt-get update
  if [ -n "$VERSION" ]; then
    sudo apt-get install -y "python$VERSION" "python$VERSION-venv"
  else
    sudo apt-get install -y python3 python3-pip python3-venv
  fi
  exit 0
fi

if command -v dnf >/dev/null 2>&1; then
  if [ -n "$VERSION" ]; then
    run_cmd sudo dnf install -y "python$VERSION" python3-pip
  else
    run_cmd sudo dnf install -y python3 python3-pip
  fi
  exit 0
fi

if command -v yum >/dev/null 2>&1; then
  run_cmd sudo yum install -y python3 python3-pip
  exit 0
fi

if command -v pacman >/dev/null 2>&1; then
  run_cmd sudo pacman -S --needed python python-pip
  exit 0
fi

if command -v zypper >/dev/null 2>&1; then
  run_cmd sudo zypper install -y python3 python3-pip
  exit 0
fi

if command -v apk >/dev/null 2>&1; then
  run_cmd sudo apk add python3 py3-pip
  exit 0
fi

echo "error: no supported package manager found" >&2
exit 1
