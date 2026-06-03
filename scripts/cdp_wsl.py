from __future__ import annotations

import contextlib
import logging
import os
import shutil
import subprocess
import time
from collections.abc import Iterator
from urllib.parse import urlparse, urlunparse


CDP_URL_ENV = "SING_BOX_TUI_CDP_URL"
WINDOWS_HOST_ENV = "WSL_WINDOWS_HOST"
WINDOWS_RELAY_ENV = "WSL_CDP_WINDOWS_RELAY"
CDP_LOG_ENV = "WSL_CDP_LOG"
LOOPBACK_HOSTS = {"127.0.0.1", "localhost", "::1"}

LOGGER = logging.getLogger(__name__)
LOGGER.addHandler(logging.NullHandler())
LOGGER_CONFIGURED = False


def configure_logging(enabled: bool | None = None) -> None:
    global LOGGER_CONFIGURED
    if enabled is None:
        enabled = _env_flag(CDP_LOG_ENV)
    if not enabled or LOGGER_CONFIGURED:
        return

    handler = logging.StreamHandler()
    handler.setFormatter(logging.Formatter("cdp_wsl: %(levelname)s: %(message)s"))
    LOGGER.addHandler(handler)
    LOGGER.setLevel(logging.INFO)
    LOGGER.propagate = False
    LOGGER_CONFIGURED = True


def _env_flag(name: str) -> bool:
    return os.environ.get(name, "").lower() in {"1", "true", "yes", "on"}


configure_logging()


@contextlib.contextmanager
def wsl_windows_cdp_url(
    cdp_url: str,
    *,
    enabled: bool,
    windows_host: str | None = None,
    relay: bool = False,
) -> Iterator[str]:
    LOGGER.info(
        "opening CDP context url=%s enabled=%s relay=%s windows_host=%s",
        cdp_url,
        enabled,
        relay,
        windows_host or "<auto>",
    )
    effective_url = resolve_wsl_windows_cdp_url(
        cdp_url,
        enabled=enabled or relay,
        windows_host=windows_host,
    )
    LOGGER.info("using effective CDP URL %s", effective_url)
    relay_process = start_windows_cdp_relay(cdp_url, effective_url) if relay else None
    try:
        yield effective_url
    finally:
        if relay_process is not None:
            stop_windows_cdp_relay(relay_process)


def resolve_wsl_windows_cdp_url(
    cdp_url: str,
    *,
    enabled: bool,
    windows_host: str | None = None,
) -> str:
    if not enabled:
        LOGGER.info("Windows CDP URL rewrite disabled; using %s", cdp_url)
        return cdp_url

    parsed = urlparse(cdp_url)
    if parsed.hostname not in LOOPBACK_HOSTS:
        LOGGER.info("CDP URL host %s is already non-loopback", parsed.hostname)
        return cdp_url

    host = windows_host or find_windows_host()
    if not host:
        raise RuntimeError(
            "could not determine the Windows host IP; pass --windows-host or set "
            f"{WINDOWS_HOST_ENV}"
        )
    rewritten = replace_url_host(cdp_url, host)
    LOGGER.info("rewrote CDP URL host %s -> %s", parsed.hostname, host)
    return rewritten


def rewrite_loopback_websocket_url(websocket_url: str, cdp_url: str) -> str:
    parsed = urlparse(websocket_url)
    cdp_host = urlparse(cdp_url).hostname
    if parsed.hostname in LOOPBACK_HOSTS and cdp_host and cdp_host not in LOOPBACK_HOSTS:
        rewritten = replace_url_host(websocket_url, cdp_host)
        LOGGER.info("rewrote CDP WebSocket host %s -> %s", parsed.hostname, cdp_host)
        return rewritten
    LOGGER.info("CDP WebSocket URL did not need host rewrite")
    return websocket_url


def find_windows_host() -> str | None:
    host = os.environ.get(WINDOWS_HOST_ENV)
    if host:
        LOGGER.info("using Windows host from $%s: %s", WINDOWS_HOST_ENV, host)
        return host

    host = _default_gateway()
    if host:
        LOGGER.info("using Windows host from WSL default gateway: %s", host)
        return host

    host = _nameserver_from_resolv_conf()
    if host:
        LOGGER.info("using Windows host from /etc/resolv.conf nameserver: %s", host)
        return host

    LOGGER.info("could not find Windows host from environment, default gateway, or resolv.conf")
    return None


def start_windows_cdp_relay(cdp_url: str, effective_url: str) -> subprocess.Popen[str]:
    parsed_cdp = urlparse(cdp_url)
    parsed_effective = urlparse(effective_url)
    listen_host = parsed_effective.hostname
    if not listen_host or listen_host in LOOPBACK_HOSTS:
        raise RuntimeError("Windows CDP relay needs a non-loopback Windows host")

    listen_port = parsed_effective.port or _default_port(parsed_effective.scheme)
    target_port = parsed_cdp.port or _default_port(parsed_cdp.scheme)
    if listen_port is None or target_port is None:
        raise RuntimeError("Windows CDP relay needs an explicit or HTTP(S) CDP port")

    powershell = _powershell_path()
    LOGGER.info(
        "starting Windows CDP relay %s:%s -> 127.0.0.1:%s using %s",
        listen_host,
        listen_port,
        target_port,
        powershell,
    )
    script = _relay_powershell_script(
        listen_host=listen_host,
        listen_port=listen_port,
        target_host="127.0.0.1",
        target_port=target_port,
    )
    process = subprocess.Popen(
        [powershell, "-NoProfile", "-Command", script],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    time.sleep(0.5)
    if process.poll() is not None:
        stdout, stderr = process.communicate(timeout=1)
        message = (stderr or stdout or "Windows CDP relay exited during startup").strip()
        LOGGER.info("Windows CDP relay failed during startup: %s", message)
        raise RuntimeError(message)
    LOGGER.info("Windows CDP relay started with pid=%s", process.pid)
    return process


def stop_windows_cdp_relay(process: subprocess.Popen[str]) -> None:
    LOGGER.info("stopping Windows CDP relay pid=%s", process.pid)
    process.terminate()
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        LOGGER.info("Windows CDP relay pid=%s did not exit; killing", process.pid)
        process.kill()
        process.wait(timeout=2)
    LOGGER.info("Windows CDP relay stopped with returncode=%s", process.returncode)


def replace_url_host(url: str, host: str) -> str:
    parsed = urlparse(url)
    port = parsed.port
    netloc = _format_netloc(host, port)
    rewritten = urlunparse(parsed._replace(netloc=netloc))
    LOGGER.info("replace URL host: %s -> %s", url, rewritten)
    return rewritten


def _default_port(scheme: str) -> int | None:
    if scheme in {"http", "ws"}:
        return 80
    if scheme in {"https", "wss"}:
        return 443
    return None


def _powershell_path() -> str:
    candidates = [
        "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe",
        "powershell.exe",
    ]
    for candidate in candidates:
        if os.path.exists(candidate):
            LOGGER.info("found PowerShell at %s", candidate)
            return candidate
        path = shutil.which(candidate)
        if path:
            LOGGER.info("found PowerShell at %s", path)
            return path
    raise RuntimeError("powershell.exe was not found; cannot start Windows CDP relay")


def _relay_powershell_script(
    *,
    listen_host: str,
    listen_port: int,
    target_host: str,
    target_port: int,
) -> str:
    return "; ".join(
        [
            '$ErrorActionPreference="Stop"',
            f"$listenAddress={_ps_quote(listen_host)}",
            f"$listenPort={listen_port}",
            f"$targetHost={_ps_quote(target_host)}",
            f"$targetPort={target_port}",
            "$listener=[System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Parse($listenAddress), $listenPort)",
            "$listener.Start()",
            (
                "[Console]::Error.WriteLine("
                '"CDP relay listening " + $listenAddress + ":" + $listenPort + '
                '" -> " + $targetHost + ":" + $targetPort)'
            ),
            (
                "while ($true) { "
                "$client=$listener.AcceptTcpClient(); "
                "$target=$null; "
                "try { "
                "$target=[System.Net.Sockets.TcpClient]::new($targetHost, $targetPort); "
                "$clientStream=$client.GetStream(); "
                "$targetStream=$target.GetStream(); "
                "$a=$clientStream.CopyToAsync($targetStream); "
                "$b=$targetStream.CopyToAsync($clientStream); "
                "[System.Threading.Tasks.Task]::WaitAny(@($a,$b)) | Out-Null "
                "} finally { "
                "if ($client -ne $null) { $client.Close() }; "
                "if ($target -ne $null) { $target.Close() } "
                "} "
                "}"
            ),
        ]
    )


def _ps_quote(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def _format_netloc(host: str, port: int | None) -> str:
    bracketed_host = f"[{host}]" if ":" in host and not host.startswith("[") else host
    return f"{bracketed_host}:{port}" if port else bracketed_host


def _nameserver_from_resolv_conf() -> str | None:
    try:
        with open("/etc/resolv.conf", encoding="utf-8") as resolv_conf:
            for line in resolv_conf:
                parts = line.split()
                if len(parts) == 2 and parts[0] == "nameserver":
                    return parts[1]
    except OSError:
        return None
    return None


def _default_gateway() -> str | None:
    try:
        result = subprocess.run(
            ["ip", "route", "show", "default"],
            check=False,
            capture_output=True,
            text=True,
            timeout=2,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None

    for line in result.stdout.splitlines():
        parts = line.split()
        if "via" in parts:
            via_index = parts.index("via")
            if via_index + 1 < len(parts):
                return parts[via_index + 1]
    return None
