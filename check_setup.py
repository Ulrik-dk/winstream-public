#!/usr/bin/env python3
"""Checks that this machine (the client) and the host have what winstream
needs, using the configuration (winstream_config.py). Prints one line per check
and what to install or set for each one that fails. Changes nothing.

    ./check_setup.py           both machines
    ./check_setup.py --local   only this one (no ssh)

Package names are Arch Linux's; other distributions name them similarly.
"""

import argparse
import importlib.util
import shutil
import subprocess
import sys

import winstream_config

OK, WARN, FAIL = "ok", "warn", "FAIL"
results = []


def report(status, what, detail="", fix=""):
    results.append(status)
    line = f"  [{status:>4}] {what}"
    if detail:
        line += f": {detail}"
    print(line)
    if fix and status != OK:
        print(f"         -> {fix}")


def have(cmd):
    return shutil.which(cmd) is not None


def run(cmd, timeout=10):
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return r.returncode == 0, (r.stdout + r.stderr).strip()
    except (OSError, subprocess.TimeoutExpired) as err:
        return False, str(err)


def check_local(cfg):
    print("This machine (client, launcher):")
    ok = sys.version_info >= (3, 11)
    report(OK if ok else FAIL, "Python 3.11+", sys.version.split()[0], "install a newer python")
    tk = importlib.util.find_spec("tkinter") is not None
    report(OK if tk else FAIL, "tkinter (launcher window)", "", "install tk")
    for cmd, fix in [("ssh", "install openssh"),
                     ("cargo", "install rustup, then: rustup default stable"),
                     ("clang", "install clang (bindgen for the FFmpeg bindings)"),
                     ("pkg-config", "install pkgconf"),
                     ("xclip", "install xclip (clipboard to and from remote windows)"),
                     ("setxkbmap", "install xorg-setxkbmap (keyboard layout for the host)")]:
        report(OK if have(cmd) else FAIL, cmd, shutil.which(cmd) or "not found", fix)
    if have("pkg-config"):
        for lib, pkg in [("libavcodec", "ffmpeg"), ("libavdevice", "ffmpeg"), ("libswscale", "ffmpeg"),
                         ("libpulse-simple", "libpulse"), ("opus", "opus")]:
            ok, out = run(["pkg-config", "--modversion", lib])
            report(OK if ok else FAIL, f"library {lib}", out if ok else "not found", f"install {pkg}")
    if have("vainfo"):
        ok, out = run(["vainfo"])
        h264 = "VAProfileH264" in out and "VAEntrypointVLD" in out
        report(OK if h264 else FAIL, "VA-API H.264 decode", "" if h264 else "no H.264 decoder listed",
               "install the VA-API driver for your GPU (intel-media-driver, libva-mesa-driver)")
    else:
        report(WARN, "VA-API H.264 decode", "vainfo not found, not checked",
               "install libva-utils to check; the client falls back to software decode")
    term = cfg.terminal[0]
    report(OK if have(term) else WARN, f"terminal {term} (Show terminals)", "" if have(term) else "not found",
           "set terminal.command in the configuration")
    if cfg.mount:
        report(OK if have("sshfs") else FAIL, "sshfs (mount.path)", "", "install sshfs, or empty mount.path")
    if cfg.ssh_key:
        exists = cfg.ssh_key.exists()
        report(OK if exists else FAIL, f"ssh.key {cfg.ssh_key}", "" if exists else "no such file",
               "fix ssh.key, or empty it")


HOST_SCRIPT = r"""
echo "cargo=$(command -v cargo || true)"
echo "rustup=$(command -v rustup || true)"
echo "xwayland=$(command -v Xwayland || true)"
echo "pactl=$(command -v pactl || true)"
echo "pkgconfig=$(command -v pkg-config || true)"
echo "runtime=${XDG_RUNTIME_DIR:-}"
echo "driver=$(cat /sys/module/nvidia/version 2>/dev/null || true)"
echo "render=$(ls /dev/dri/renderD* 2>/dev/null | tr '\n' ' ')"
for lib in libnvidia-encode.so.1 libcuda.so.1 libgbm.so.1 libxkbcommon.so.0 libopus.so.0 libpulse-simple.so.0 libEGL_nvidia.so.0; do
  if ldconfig -p 2>/dev/null | grep -q "$lib "; then echo "lib_$lib=yes"; else echo "lib_$lib="; fi
done
echo "pulse=$(timeout 5 pactl info >/dev/null 2>&1 && echo yes)"
echo "dir=$(cd @HOST_DIR@ 2>/dev/null && pwd)"
"""


def check_host(cfg):
    print(f"\nThe host ({cfg.host}, over ssh):")
    ssh = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5", cfg.host]
    cfg.use_agent()
    ok, out = run(ssh + [HOST_SCRIPT.replace("@HOST_DIR@", cfg.remote_dir)], timeout=30)
    if not ok and "=" not in out:
        report(FAIL, "ssh", out.splitlines()[-1] if out else "no answer",
               f"`ssh {cfg.host}` must work without a prompt: key in the agent, host awake "
               "(host.wake), host key accepted once by hand")
        return
    report(OK, "ssh", "answers without a prompt")
    v = dict(line.split("=", 1) for line in out.splitlines() if "=" in line)
    report(OK if v.get("cargo") else FAIL, "cargo on PATH for ssh commands", v.get("cargo") or "not found",
           "install rustup; make sure ~/.cargo/bin is on PATH for non-interactive ssh (e.g. ~/.bashrc "
           "before its interactive check, or ~/.ssh/environment)")
    report(OK if v.get("pkgconfig") else FAIL, "pkg-config", "", "install pkgconf")
    report(OK if v.get("xwayland") else FAIL, "Xwayland", v.get("xwayland") or "not found",
           "install xorg-xwayland (X11 apps: Steam, most games)")
    driver = v.get("driver", "")
    if not driver:
        report(FAIL, "NVIDIA driver", "nvidia module not loaded", "install and load the NVIDIA driver")
    else:
        major = int(driver.split(".")[0]) if driver.split(".")[0].isdigit() else 0
        report(OK if major >= 575 else WARN, "NVIDIA driver", driver,
               "tested with 580; the server uses the CUDA 12.9 API (driver 575 or newer)")
    report(OK if v.get("render") else FAIL, "GPU render node", v.get("render", "").strip() or "none",
           "the user needs access to /dev/dri/renderD* (group render or video)")
    for lib, pkg in [("libnvidia-encode.so.1", "nvidia-utils (NVENC)"), ("libcuda.so.1", "nvidia-utils (CUDA)"),
                     ("libEGL_nvidia.so.0", "nvidia-utils (EGL)"), ("libgbm.so.1", "mesa"),
                     ("libxkbcommon.so.0", "libxkbcommon"), ("libopus.so.0", "opus"),
                     ("libpulse-simple.so.0", "libpulse")]:
        report(OK if v.get(f"lib_{lib}") else FAIL, f"library {lib}", "", f"install {pkg}")
    report(OK if v.get("runtime") else FAIL, "XDG_RUNTIME_DIR in ssh sessions", v.get("runtime") or "unset",
           "log in through a session manager (elogind/systemd-logind) so ssh sessions get one")
    report(OK if v.get("pulse") else FAIL, "PulseAudio/PipeWire for this user", "" if v.get("pulse") else
           "`pactl info` fails over ssh",
           "run pipewire + pipewire-pulse (or pulseaudio) for the user, e.g. through an auto-login "
           "session or user services that start without a login")
    report(OK if v.get("dir") else WARN, f"host.dir {cfg.host_dir}", v.get("dir") or "not there yet",
           "created by the first build (\"Build server\" or ./dev.sh)")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--local", action="store_true", help="only check this machine")
    args = ap.parse_args()
    try:
        cfg = winstream_config.load()
    except winstream_config.ConfigError as err:
        print(f"error: {err}\nCopy {winstream_config.EXAMPLE} there and edit it.")
        return 1
    print(f"Configuration: {cfg.path}\n  host {cfg.host}, client connects to {" or ".join(cfg.client_targets())}\n")
    check_local(cfg)
    if not args.local:
        check_host(cfg)
    failed = results.count(FAIL)
    print(f"\n{failed} failed, {results.count(WARN)} warnings" if failed or WARN in results else "\nall ok")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
