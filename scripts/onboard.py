#!/usr/bin/env python3
"""Onboard local dependencies for sing-box-tui."""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import stat
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request
import zipfile
from pathlib import Path


RELEASE_API_URL = "https://api.github.com/repos/SagerNet/sing-box/releases/latest"
USER_AGENT = "sing-box-tui-onboarding"


def main() -> int:
    args = parse_args()
    existing = shutil.which("sing-box")
    if args.check_only:
        if existing:
            print(f"sing-box already found: {existing}")
            return 0
        print("sing-box was not found on PATH")
        return 1
    if existing and not args.force:
        print(f"sing-box already found: {existing}")
        return 0

    install_dir = args.install_dir or default_install_dir()
    os_name = detect_os()
    arch = detect_arch()
    release = fetch_latest_release(args.version)
    asset = select_asset(release, os_name, arch)
    if args.dry_run:
        print(f"would install {asset['name']} to {install_dir}")
        return 0

    install_dir.mkdir(parents=True, exist_ok=True)
    binary_path = install_sing_box_asset(asset, install_dir)
    print(f"installed sing-box: {binary_path}")
    if str(install_dir) not in os.environ.get("PATH", "").split(os.pathsep):
        print(path_hint(install_dir))
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Install sing-box if it is not already available on PATH."
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="install even when sing-box already exists on PATH",
    )
    parser.add_argument(
        "--check-only",
        action="store_true",
        help="only check whether sing-box exists on PATH",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="resolve the release asset without downloading or installing it",
    )
    parser.add_argument(
        "--install-dir",
        type=Path,
        help="directory to install sing-box into (default: user-local bin directory)",
    )
    parser.add_argument(
        "--version",
        help="install a specific sing-box tag such as v1.13.13 (default: latest)",
    )
    return parser.parse_args()


def default_install_dir() -> Path:
    if sys.platform == "win32":
        base = os.environ.get("LOCALAPPDATA")
        if base:
            return Path(base) / "sing-box-tui" / "bin"
        return Path.home() / "AppData" / "Local" / "sing-box-tui" / "bin"
    return Path.home() / ".local" / "bin"


def detect_os() -> str:
    if sys.platform == "win32":
        return "windows"
    if sys.platform == "darwin":
        return "darwin"
    if sys.platform.startswith("linux"):
        return "linux"
    raise RuntimeError(f"unsupported OS for sing-box installer: {sys.platform}")


def detect_arch() -> str:
    machine = platform.machine().lower()
    aliases = {
        "amd64": "amd64",
        "x86_64": "amd64",
        "i386": "386",
        "i686": "386",
        "aarch64": "arm64",
        "arm64": "arm64",
        "armv7l": "armv7",
        "armv6l": "armv6",
    }
    arch = aliases.get(machine)
    if not arch:
        raise RuntimeError(f"unsupported CPU architecture for sing-box installer: {machine}")
    return arch


def fetch_latest_release(version: str | None) -> dict:
    url = RELEASE_API_URL
    if version:
        url = f"https://api.github.com/repos/SagerNet/sing-box/releases/tags/{version}"
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"failed to fetch sing-box release metadata: HTTP {error.code}") from error
    except urllib.error.URLError as error:
        raise RuntimeError(f"failed to fetch sing-box release metadata: {error}") from error


def select_asset(release: dict, os_name: str, arch: str) -> dict:
    assets = release.get("assets") or []
    names = [asset.get("name", "") for asset in assets]
    extension = "zip" if os_name == "windows" else "tar.gz"
    preferred = [
        f"sing-box-*-{os_name}-{arch}.{extension}",
        f"sing-box-*-{os_name}-{arch}-glibc.{extension}",
        f"sing-box-*-{os_name}-{arch}-musl.{extension}",
    ]
    if arch == "armv7":
        preferred.insert(0, f"sing-box-*-{os_name}-armv7.{extension}")
        preferred.append(f"sing-box-*-{os_name}-arm.{extension}")
    if arch == "armv6":
        preferred.insert(0, f"sing-box-*-{os_name}-armv6.{extension}")
        preferred.append(f"sing-box-*-{os_name}-arm-softfloat.{extension}")

    for pattern in preferred:
        for asset in assets:
            name = asset.get("name", "")
            if matches_asset_pattern(name, pattern):
                return asset

    wanted = f"{os_name}-{arch}"
    available = "\n  ".join(name for name in names if name.startswith("sing-box-"))
    raise RuntimeError(
        f"could not find sing-box release asset for {wanted}. Available sing-box assets:\n  {available}"
    )


def matches_asset_pattern(name: str, pattern: str) -> bool:
    prefix, suffix = pattern.split("*", 1)
    return name.startswith(prefix) and name.endswith(suffix)


def install_sing_box_asset(asset: dict, install_dir: Path) -> Path:
    url = asset.get("browser_download_url")
    name = asset.get("name")
    if not url or not name:
        raise RuntimeError("release asset is missing download URL or name")
    with tempfile.TemporaryDirectory(prefix="sing-box-tui-onboard-") as tmp:
        archive_path = Path(tmp) / name
        download(url, archive_path)
        extracted_binary = extract_sing_box_binary(archive_path, Path(tmp))
        target = install_dir / ("sing-box.exe" if sys.platform == "win32" else "sing-box")
        shutil.copy2(extracted_binary, target)
        if sys.platform != "win32":
            target.chmod(target.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
        return target


def download(url: str, destination: Path) -> None:
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            with destination.open("wb") as output:
                shutil.copyfileobj(response, output)
    except urllib.error.URLError as error:
        raise RuntimeError(f"failed to download {url}: {error}") from error


def extract_sing_box_binary(archive_path: Path, destination: Path) -> Path:
    if archive_path.suffix == ".zip":
        with zipfile.ZipFile(archive_path) as archive:
            safe_extract_zip(archive, destination)
    else:
        with tarfile.open(archive_path) as archive:
            safe_extract_tar(archive, destination)
    binary_names = {"sing-box", "sing-box.exe"}
    matches = [
        path
        for path in destination.rglob("*")
        if path.is_file() and path.name in binary_names
    ]
    if not matches:
        raise RuntimeError(f"archive did not contain a sing-box executable: {archive_path.name}")
    return matches[0]


def safe_extract_zip(archive: zipfile.ZipFile, destination: Path) -> None:
    destination = destination.resolve()
    for member in archive.infolist():
        target = (destination / member.filename).resolve()
        if not is_relative_to(target, destination):
            raise RuntimeError(f"refusing unsafe zip member path: {member.filename}")
    archive.extractall(destination)


def safe_extract_tar(archive: tarfile.TarFile, destination: Path) -> None:
    destination = destination.resolve()
    for member in archive.getmembers():
        target = (destination / member.name).resolve()
        if not is_relative_to(target, destination):
            raise RuntimeError(f"refusing unsafe tar member path: {member.name}")
    archive.extractall(destination)


def is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


def path_hint(install_dir: Path) -> str:
    if sys.platform == "win32":
        return (
            "add this directory to PATH for future shells:\n"
            f"  setx PATH \"%PATH%;{install_dir}\""
        )
    return (
        "add this directory to PATH for future shells, for example:\n"
        f"  export PATH=\"{install_dir}:$PATH\""
    )


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RuntimeError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
