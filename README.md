# winstream

Window streaming for Linux apps (very relevant for games). Apps render on
one machine's GPU (the **host**); each of their windows appears on another
machine (the **client**) as a native window, streamed as low-latency video
with sound. Keyboard, mouse and clipboard input go back to the host.

Measured at 2560x1440, 60 fps, over 5 GHz Wi-Fi:
- **~15 ms** from the app drawing a frame to the client showing it
  (p95 ~25 ms); sound ~50 ms.
- On a congested link, the bitrate drops to keep latency and frame rate.

Existing tools give either native windows or game-grade streaming, not both:
- Game streamers and remote desktops (Moonlight, Steam Remote Play, Parsec,
  RDP) send one picture of a whole screen, often at a fixed resolution.
- Window forwarders (ssh -X, waypipe, Xpra) give native windows, but lack
  some of GPU rendering, sound, adaptive bitrate or Wayland apps.

## Comparison

| | Client native windows | Headless host | Host GPU | Sound | Adaptive bitrate | X11 client | Wayland + X11 apps | Free + Linux |
|---|---|---|---|---|---|---|---|---|
| Sunshine + Moonlight | ❌ | ❌ | ✅ | ✅ | ❌ | ✅ | ✅ | ✅ |
| Moonshine, Wolf | ❌ | ✅ | ✅ | ✅ | ❌ | ✅ | ✅ | ✅ |
| Steam Remote Play | ❌ | 🟡 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Parsec | ❌ | ❌ | ✅ | ✅ | ✅ | ✅ | ✅ | ❌ |
| RDP, VNC | ❌ | 🟡 | 🟡 | 🟡 | 🟡 | ✅ | 🟡 | ✅ |
| NoMachine | 🟡 | ❌ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| ssh -X, x2go | ✅ | ✅ | ❌ | 🟡 | ❌ | ✅ | ❌ | ✅ |
| Xpra | ✅ | ✅ | 🟡 | ✅ | ✅ | ✅ | ❌ | ✅ |
| waypipe | ✅ | ✅ | ✅ | ❌ | ❌ | ❌ | 🟡 | ✅ |
| **winstream** | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |

- **Client native windows**: each remote window is its own native window on
  the client, and can be tiled and resized freely.
- **Headless host**: no desktop environment, monitor or dummy plug.
- **Host GPU**: apps render on the host's GPU, so 3D runs at full speed
  (not drawing commands sent over the wire).
- **Sound**: the app's audio plays on the client.
- **Adaptive bitrate**: video compression quality adapts to a target fps
  and latency.
- **X11 client**: no Wayland compositor needed on the client.
- **Wayland + X11 apps**: the host runs both kinds of app.
- **Free + Linux**: free to use, with host and client on Linux.

## How it works

- **server**: a headless Wayland compositor (Smithay) with XWayland.
  Apps run inside it, so anything they spawn does too. Being the compositor,
  it gets each window's buffer directly: **no screen capture, and frames
  never leave the GPU** (GL → CUDA → NVENC, no copy through CPU RAM). Encoding starts as soon as the app commits a frame. Audio is
  captured from a null sink and sent as Opus.
- **client**: one native window per remote window; VA-API decode straight
  into an EGL texture; input capture incl. pointer lock.
- **transport**: QUIC (quinn) over UDP: control stream, one video stream,
  audio as datagrams.
- **launcher.py** (client): a small optional tkinter panel that wakes the host,
  builds, starts server and client, and starts apps. A wrapper for the CLI.

Design notes:
- NVENC is called directly, not through FFmpeg: FFmpeg's NVENC encoder
  needs a newer driver than Pascal GPUs can run (580, NVENC API 13.0).
- Stale frames are skipped on both sides (raw before encode, decoded before
  display), and the encoder stops while the link is stalled.
  Recovery after a 2 s cut: ~1 s.
- No explicit sync (`linux-drm-syncobj`) yet. Flicker or half-drawn frames
  in an app would point there.

## Requirements

Tested on Artix Linux (Arch packages) on both machines.

**Host** (tested: GTX 1050 Ti, driver 580.196.07):
- An NVIDIA GPU with NVENC H.264 and the proprietary driver 575 or newer.
  No monitor needed. AMD and Intel are not supported.
- `nvidia-utils`, `mesa`, `xorg-xwayland`, `libxkbcommon`, `opus`,
  `libpulse`, `pkgconf`.
- PipeWire (`pipewire-pulse`) or PulseAudio, reachable by `pactl` from ssh.
  While the server runs, its sink is the default.
- Rust (`rustup`, stable), `cargo` on the PATH of non-interactive ssh.
- An ssh server, and `XDG_RUNTIME_DIR` set in ssh sessions (elogind or
  systemd-logind).

**Client** (tested: Intel UHD 620, i3 on X11):
- An X11 session (Wayland untested; `xclip` and `setxkbmap` are X11 tools).
- VA-API H.264 decode (`intel-media-driver` or `libva-mesa-driver`); else
  software decode, slower.
- `ffmpeg` 9, `libpulse`, `opus`, `xclip`, `xorg-setxkbmap`, `clang`,
  `pkgconf`, Rust.
- Python 3.11+ with `tk`; `sshfs` to mount the host's home.

## Security

Like X11's `.Xauthority`: each server run makes a new certificate and a
random token and writes both to `/run/user/UID/.WinstreamAuthority-PORT` on
the host (your runtime folder: RAM, readable only by you; removed when the
server stops; `host.authority` or `--authority PATH` to put it elsewhere). The
client reads it over ssh (`client --ssh HOST SERVER`), accepts only that
certificate and sends the token first; the server drops any connection
without it. So only someone who can ssh into the host as you can connect.
Traffic is encrypted (QUIC, TLS 1.3).

## Setup

1. Install the requirements; make `ssh HOST` work without a prompt.
2. Clone this repository on the client.
3. Copy `config.example.toml` to `~/.config/winstream/config.toml` and set
   `host.ssh` (the launcher does the copy if it's missing).
4. `./check_setup.py` checks both machines.
5. `./launcher.py`, then "Start session". Add apps with "+ Add app"
   (kept in `~/.config/winstream/apps.json`).

### Configuration

| Setting | Default | What it is |
|---|---|---|
| `host.ssh` | (required) | ssh destination of the host |
| `host.dir` | `~/winstream` | where the code is copied and built on the host |
| `host.port` | `4433` | server port (UDP) |
| `host.wake` | none | command run on the client to wake the host |
| `host.authority` | `/run/user/{uid}/.WinstreamAuthority-{port}` | where the server writes its certificate fingerprint and token |
| `client.server` | ssh's HostName for `host.ssh` | address the client connects to, or a list tried in order (LAN first, then e.g. Tailscale) |
| `ssh.key` | none | key the launcher unlocks when the agent lacks it |
| `ssh.agent_socket` | `$SSH_AUTH_SOCK` | ssh-agent socket to use |
| `mount.path` | none | where to mount the host's home (sshfs) |
| `stream.fps` | `60` | frame rate apps are asked for |
| `stream.latency_ms` | `15` | encode-to-shown target the bitrate adapts to |
| `stream.start_bits_per_pixel` | `0.08` | starting bitrate |
| `stream.max_bits_per_pixel` | `1.0` | bitrate ceiling |
| `stream.min_mbit` | `2` | bitrate floor, Mbit/s |
| `terminal.command` | `xterm -hold -T {title} -e` | terminal for "Show terminals (debug)" |
