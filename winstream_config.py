#!/usr/bin/env python3
"""Reads the winstream configuration: which machine runs the apps (the
host), how to reach and wake it, where its home is mounted here, and which
terminal to use. Everything specific to one setup lives in this file, not
in the repository:

    $XDG_CONFIG_HOME/winstream/config.toml   (default ~/.config/winstream/)
    $WINSTREAM_CONFIG                                  (another file, if set)

config.example.toml in the repository lists every setting. The launcher
keeps its app buttons in apps.json next to config.toml.

    ./winstream_config.py            print the configuration as read, with defaults
    ./winstream_config.py shell      the same as shell variables (for dev.sh)
"""

import os
import shlex
import subprocess
import sys
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

REPO = Path(__file__).resolve().parent
EXAMPLE = REPO / "config.example.toml"
DEFAULT_PORT = 4433  # proto::DEFAULT_PORT


def config_dir():
    base = os.environ.get("XDG_CONFIG_HOME") or str(Path.home() / ".config")
    return Path(base) / "winstream"


def config_path():
    return Path(os.environ["WINSTREAM_CONFIG"]) if os.environ.get("WINSTREAM_CONFIG") else config_dir() / "config.toml"


def expand(path):
    """~ and $VARS, for paths on this machine."""
    return Path(os.path.expandvars(os.path.expanduser(path))) if path else None


def remote_path(path):
    """A path on the host as a shell word for the host's shell: quoted, with
    a leading ~ turned into "$HOME" (a quoted ~ wouldn't expand)."""
    if path == "~":
        return '"$HOME"'
    if path.startswith("~/"):
        return '"$HOME"/' + shlex.quote(path[2:])
    return shlex.quote(path)


class ConfigError(Exception):
    pass


@dataclass
class Config:
    path: Path
    host: str
    host_dir: str = "~/winstream"
    port: int = DEFAULT_PORT
    wake: str = ""
    authority: str = ""
    server_addrs: list = field(default_factory=list)
    ssh_key: Path | None = None
    ssh_agent_socket: Path | None = None
    mount: Path | None = None
    terminal: list = field(default_factory=lambda: ["xterm", "-hold", "-T", "{title}", "-e"])
    latency_ms: int = 15
    fps: int = 60
    start_bpp: float = 0.08
    max_bpp: float = 1.0
    min_mbit: float = 2.0

    @property
    def apps_file(self):
        return self.path.parent / "apps.json"

    @property
    def remote_dir(self):
        return remote_path(self.host_dir)

    def client_targets(self):
        """What the client connects to, tried in order: client.server (one
        address or a list), or else the address ssh uses for the host (its
        HostName), each with the port if it isn't the default."""
        addrs = self.server_addrs or [ssh_hostname(self.host)]
        if self.port != DEFAULT_PORT:
            addrs = [a if ":" in a else f"{a}:{self.port}" for a in addrs]
        return addrs

    def use_agent(self):
        if self.ssh_agent_socket and self.ssh_agent_socket.exists():
            os.environ["SSH_AUTH_SOCK"] = str(self.ssh_agent_socket)


def ssh_hostname(host):
    try:
        out = subprocess.run(["ssh", "-G", host], capture_output=True, text=True, timeout=5).stdout
    except (OSError, subprocess.TimeoutExpired):
        return host
    for line in out.splitlines():
        key, _, value = line.partition(" ")
        if key == "hostname" and value:
            return value
    return host


def load(path=None):
    path = Path(path) if path else config_path()
    try:
        data = tomllib.loads(path.read_text())
    except FileNotFoundError:
        raise ConfigError(f"no configuration at {path}") from None
    except tomllib.TOMLDecodeError as err:
        raise ConfigError(f"{path}: {err}") from None

    def section(name):
        value = data.get(name, {})
        if not isinstance(value, dict):
            raise ConfigError(f"{path}: [{name}] must be a table")
        return value

    host, client, ssh, mount = section("host"), section("client"), section("ssh"), section("mount")
    terminal, stream = section("terminal"), section("stream")
    if not host.get("ssh"):
        raise ConfigError(f"{path}: host.ssh is required (the ssh destination of the machine that runs the apps)")
    cfg = Config(path=path, host=host["ssh"])
    cfg.host_dir = host.get("dir", cfg.host_dir)
    cfg.port = int(host.get("port", cfg.port))
    cfg.wake = host.get("wake", "")
    cfg.authority = host.get("authority", "")
    server = client.get("server", "")
    cfg.server_addrs = [server] if isinstance(server, str) and server else list(server or [])
    cfg.ssh_key = expand(ssh.get("key", ""))
    cfg.ssh_agent_socket = expand(ssh.get("agent_socket", ""))
    cfg.mount = expand(mount.get("path", ""))
    if "command" in terminal:
        if not isinstance(terminal["command"], list) or not terminal["command"]:
            raise ConfigError(f"{path}: terminal.command must be a list, e.g. [\"xterm\", \"-hold\", \"-e\"]")
        cfg.terminal = terminal["command"]
    cfg.latency_ms = int(stream.get("latency_ms", cfg.latency_ms))
    cfg.fps = int(stream.get("fps", cfg.fps))
    cfg.start_bpp = float(stream.get("start_bits_per_pixel", cfg.start_bpp))
    cfg.max_bpp = float(stream.get("max_bits_per_pixel", cfg.max_bpp))
    cfg.min_mbit = float(stream.get("min_mbit", cfg.min_mbit))
    if cfg.latency_ms <= 0 or cfg.fps <= 0 or cfg.min_mbit <= 0:
        raise ConfigError(f"{path}: stream.latency_ms, stream.fps and stream.min_mbit must be more than 0")
    if not 0 < cfg.start_bpp <= cfg.max_bpp:
        raise ConfigError(f"{path}: need 0 < stream.start_bits_per_pixel <= stream.max_bits_per_pixel")
    return cfg


def main():
    try:
        cfg = load()
    except ConfigError as err:
        print(f"error: {err}\nCopy {EXAMPLE} there and edit it.", file=sys.stderr)
        return 1
    values = {
        "config": cfg.path,
        "host": cfg.host,
        "host_dir": cfg.host_dir,
        "port": cfg.port,
        "wake": cfg.wake,
        "authority": cfg.authority,
        "client_targets": " ".join(cfg.client_targets()),
        "ssh_key": cfg.ssh_key or "",
        "ssh_agent_socket": cfg.ssh_agent_socket or "",
        "mount": cfg.mount or "",
        "terminal": shlex.join(cfg.terminal),
        "fps": cfg.fps,
        "latency_ms": cfg.latency_ms,
        "start_bits_per_pixel": cfg.start_bpp,
        "max_bits_per_pixel": cfg.max_bpp,
        "min_mbit": cfg.min_mbit,
        "apps_file": cfg.apps_file,
    }
    if sys.argv[1:] == ["shell"]:
        values["remote_dir"] = cfg.remote_dir
        for key, value in values.items():
            print(f"WINSTREAM_{key.upper()}={shlex.quote(str(value))}")
    else:
        for key, value in values.items():
            print(f"{key:21} {value}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
