#!/usr/bin/env python3
"""winstream launcher: wake the host, build, start the server, open the
client, start apps. One button per stage; the status to its right says
whether the stage is done ("OK"), being checked or run, or failed.

Settings (host, wake command, mount, ssh key, terminal) come from the
configuration file, see winstream_config.py and config.example.toml. With none,
the launcher copies the example there and says so.

The apps are a column on the right, kept in apps.json next to the
configuration: "+ Add app" adds one, ✎ edits or deletes it (name, folder
on the host, command, running check, and whether "Start session" starts it).

With mount.path set, the host's home is mounted there (sshfs) for file
transfer; it stays mounted after the launcher closes.

Nothing happens on startup; statuses stay empty until "Re-check" or a
stage is pressed. With ssh.key set, any button that needs ssh first asks
for that key's passphrase in a small window if the agent doesn't hold it.

"Start session" goes through the stages in order and skips the ones already OK,
then starts the apps ticked for it.
When a stage's task ends, that stage is checked again.

The server and the client run in the background (output in the log pane
and ~/.local/state/winstream/launcher-*.log); tick "Show terminals
(debug)" to run them in terminals instead. "Show overlay" shows or hides
the fps/latency text in the remote windows, also while they're open.
The commands (HOST is host.ssh, DIR host.dir, ADDR client.server):
    ssh HOST 'cd DIR && ./target/release/server stream'
    ./target/release/client --ssh HOST ADDR
The server starts with no app; app buttons start apps in it:
    ssh HOST 'DIR/target/release/server launch -- sh -c "cd FOLDER && exec COMMAND"'
"Stop" stops the apps started in the session (30 s to save, then killed),
then the server, then the client, and closes the terminals. Closing the
launcher leaves everything running.

    ./launcher.py
"""

import hashlib
import json
import os
import queue
import re
import shlex
import subprocess
import sys
import threading
import tkinter as tk
from pathlib import Path
from tkinter import filedialog, messagebox, ttk
from tkinter.scrolledtext import ScrolledText

import winstream_config

HERE = Path(__file__).resolve().parent
CFG = None
HOST = REMOTE_DIR = None
CLIENT_TARGETS = []
PORT = winstream_config.DEFAULT_PORT
MOUNT = APPS_FILE = None
# Apps: the buttons in the right-hand column, added and edited in the
# launcher (✎ / "+ Add app") and kept in apps.json. Each one has:
#   name       the button says "Start <name>"
#   dir        folder on the host it starts in ("~/..." is fine); may be empty
#   command    command line on the host, as typed in a shell
#   check      pgrep arguments that find it running; empty: -x <program name>
#   start_with_session  "Start session" starts it after the client
# A check by command line (-f) should be anchored ("-f '^godot-mono '"), so
# the shell ssh starts for the check doesn't match itself.
DEFAULT_APPS = []
SSH = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=4"]
LOG_DIR = Path.home() / ".local" / "state" / "winstream"
# While this exists the client hides its fps/latency overlay ("Show overlay").
OVERLAY_OFF_FLAG = Path(os.environ.get("XDG_RUNTIME_DIR", "/tmp")) / "winstream-overlay-off"
ANSI = re.compile(r"\x1b\[[0-9;]*m")

# Files that go into each binary; the build stamp is a hash of them.
SHARED = ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo", "proto"]
SERVER_SOURCES = SHARED + ["server"]
CLIENT_SOURCES = SHARED + ["client"]
SERVER_STAMP = "target/release/server.stamp"
CLIENT_STAMP = HERE / "target/release/client.stamp"


def configure(cfg):
    global CFG, HOST, REMOTE_DIR, CLIENT_TARGETS, PORT, MOUNT, APPS_FILE
    CFG = cfg
    HOST, REMOTE_DIR, PORT = cfg.host, cfg.remote_dir, cfg.port
    CLIENT_TARGETS = cfg.client_targets()
    MOUNT, APPS_FILE = cfg.mount, cfg.apps_file
    cfg.use_agent()


def load_config():
    """The configuration, or None after telling the user what to do. With
    no file yet, the example is copied there to edit."""
    path = winstream_config.config_path()
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(winstream_config.EXAMPLE.read_text())
        message = (f"No configuration yet. The example is now at\n{path}\n\n"
                   "Set host.ssh (and whatever else applies), then start the launcher again.")
    else:
        try:
            return winstream_config.load(path)
        except winstream_config.ConfigError as err:
            message = str(err)
    print(message, file=sys.stderr)
    root = tk.Tk(className="winstream-launcher")
    root.withdraw()
    messagebox.showinfo("winstream", message)
    root.destroy()
    return None


def askpass(prompt):
    """SSH_ASKPASS mode: ssh-add runs this file to ask for the key's
    passphrase. One small dialog; prints the answer, exit 1 if cancelled."""
    root = tk.Tk(className="winstream-askpass")
    root.title("winstream: unlock SSH key")
    try:
        root.attributes("-type", "dialog")   # i3 floats dialogs
    except tk.TclError:
        pass
    frame = ttk.Frame(root, padding=12)
    frame.pack(fill="both", expand=True)
    ttk.Label(frame, text="The launcher needs your SSH key to reach the host.",
              wraplength=380).pack(anchor="w")
    ttk.Label(frame, text=prompt, wraplength=380).pack(anchor="w", pady=(6, 4))
    answer = tk.StringVar()
    secret = "yes/no" not in prompt   # host-key questions are answered in clear text
    entry = ttk.Entry(frame, textvariable=answer, show="•" if secret else "", width=42)
    entry.pack(fill="x")
    result = {}

    def ok(_=None):
        result["answer"] = answer.get()
        root.destroy()

    buttons = ttk.Frame(frame)
    buttons.pack(anchor="e", pady=(10, 0))
    ttk.Button(buttons, text="Cancel", command=root.destroy).pack(side="right")
    ttk.Button(buttons, text="Unlock", command=ok).pack(side="right", padx=(0, 6))
    root.bind("<Return>", ok)
    root.bind("<Escape>", lambda _: root.destroy())
    root.attributes("-topmost", True)
    root.after(100, entry.focus_force)
    root.mainloop()
    if "answer" not in result:
        return 1
    print(result["answer"])
    return 0


def load_apps():
    try:
        return json.loads(APPS_FILE.read_text())
    except FileNotFoundError:
        return [dict(app) for app in DEFAULT_APPS]


def save_apps(apps):
    APPS_FILE.parent.mkdir(parents=True, exist_ok=True)
    APPS_FILE.write_text(json.dumps(apps, indent=2) + "\n")


def app_launch_command(app):
    """What `server launch --` runs: the command, in its folder if it has one
    (exec, so the app keeps the shell's pid and the server tracks it)."""
    folder = app["dir"].strip()
    if not folder:
        inner = f"exec {app['command']}"
    else:
        # Quoted, so spaces work; a leading ~ becomes "$HOME" (a quoted ~ wouldn't expand).
        if folder == "~" or folder.startswith("~/"):
            cd = '"$HOME"' + (shlex.quote(folder[1:]) if len(folder) > 1 else "")
        else:
            cd = shlex.quote(folder)
        inner = f"cd {cd} && exec {app['command']}"
    return f"sh -c {shlex.quote(inner)}"


def app_check(app):
    if app["check"].strip():
        return app["check"]
    program = Path(shlex.split(app["command"])[0]).name
    return f"-x {shlex.quote(program[:15])}"  # process names are cut at 15 characters


def client_pattern():
    """pgrep -f pattern for our client: anchored, so the terminal or shell
    whose command lines contain the same text don't match."""
    return "^./target/release/client " + re.escape(client_args()) + "$"


def authority_args():
    return ["--authority", CFG.authority] if CFG.authority else []


def client_args():
    return shlex.join(["--ssh", HOST, *authority_args(), *CLIENT_TARGETS])


def sources_hash(parts):
    h = hashlib.sha256()
    for part in parts:
        p = HERE / part
        files = sorted(f for f in p.rglob("*") if f.is_file()) if p.is_dir() else [p]
        for f in files:
            if f.exists():
                h.update(str(f.relative_to(HERE)).encode())
                h.update(f.read_bytes())
    return h.hexdigest()[:16]


class Stage:
    def __init__(self, name, check, action):
        self.name, self.check, self.action = name, check, action
        self.status = None  # tk.StringVar, set by the UI
        self.button = None


class Launcher:
    def __init__(self, root):
        self.root = root
        self.events = queue.Queue()
        self.busy = False
        self.server_proc = None
        self.client_proc = None

        root.title("winstream")
        frame = ttk.Frame(root, padding=10)
        frame.pack(fill="both", expand=True)

        self.session_button = ttk.Button(frame, text="Start session", width=18,
                                         command=lambda: self.start(self.start_session))
        self.session_button.grid(row=0, column=0, sticky="w", pady=(0, 8))

        self.stages = [Stage(f"Wake {HOST}", self.check_awake, self.wake)]
        if MOUNT:
            self.stages.append(Stage("Mount home", self.check_mounted, self.mount))
        self.stages += [
            Stage("Build server", self.check_server_built, self.build_server),
            Stage("Build client", self.check_client_built, self.build_client),
            Stage("Start server", self.check_server_running, self.start_server),
            Stage("Open client", self.check_client_running, self.open_client),
        ]
        self.server_stage, self.client_stage = self.stages[-2:]
        for i, stage in enumerate(self.stages, start=1):
            stage.button = ttk.Button(frame, text=stage.name, width=18,
                                      command=lambda s=stage: self.start(self.run_stage, s))
            stage.button.grid(row=i, column=0, sticky="w", pady=2)
            stage.status = tk.StringVar(value="")
            ttk.Label(frame, textvariable=stage.status, width=14).grid(row=i, column=1, sticky="w", padx=8)

        bar = ttk.Frame(frame)
        bar.grid(row=len(self.stages) + 2, column=0, columnspan=3, sticky="w", pady=8)
        ttk.Button(bar, text="Re-check", command=lambda: self.start(self.check_all)).pack(side="left", padx=(0, 4))
        ttk.Button(bar, text="Stop", command=self.stop).pack(side="left")
        self.show_overlay = tk.BooleanVar(value=not OVERLAY_OFF_FLAG.exists())
        ttk.Checkbutton(bar, text="Show overlay", variable=self.show_overlay,
                        command=self.toggle_overlay).pack(side="left", padx=(12, 0))
        self.show_terminals = tk.BooleanVar(value=False)
        ttk.Checkbutton(bar, text="Show terminals (debug)", variable=self.show_terminals).pack(side="left", padx=(12, 0))

        self.log_box = ScrolledText(frame, height=14, width=90, state="disabled", wrap="none")
        self.log_box.grid(row=len(self.stages) + 3, column=0, columnspan=3, sticky="nsew")
        frame.columnconfigure(1, weight=1)
        frame.rowconfigure(len(self.stages) + 3, weight=1)

        # Apps column, to the right of the stages.
        self.apps = load_apps()
        self.app_stages, self.app_buttons = [], []
        self.app_frame = ttk.LabelFrame(frame, text="Apps", padding=(8, 4))
        self.app_frame.grid(row=0, column=2, rowspan=len(self.stages) + 1, sticky="nw", padx=(16, 0))
        self.build_apps()

        root.protocol("WM_DELETE_WINDOW", self.quit)
        root.after(50, self.pump)
        root.after(1000, self.watch_processes)
        # Nothing runs on startup: no checks, no passphrase prompt. The first
        # button pressed asks for the key if needed.

    # --- Threading: work runs in a thread, UI updates go through a queue ---

    def start(self, fn, *args):
        if self.busy:
            return
        self.busy = True
        self.set_buttons("disabled")

        def run():
            try:
                if not self.ensure_keys():
                    for stage in self.stages + self.app_stages:
                        self.set_status(stage, "–")
                    return
                fn(*args)
            except Exception as err:  # keep the UI usable whatever happens
                self.log(f"error: {err}")
            finally:
                self.events.put(("done", None))

        threading.Thread(target=run, daemon=True).start()

    def ensure_keys(self):
        """Every stage reaches the host over ssh without prompts (BatchMode),
        so a locked key looks like "the host is asleep". If the agent doesn't
        hold ssh.key, ask for its passphrase in a small window (this file in
        askpass mode) and add it. No network needed for that. Without
        ssh.key, keys are left to the user."""
        key = CFG.ssh_key
        if key is None or self.key_loaded(key):
            return True
        state = subprocess.run(["ssh-add", "-l"], capture_output=True).returncode
        sock = CFG.ssh_agent_socket
        if state == 2 and sock:   # no agent answering: start the shared one
            sock.unlink(missing_ok=True)
            subprocess.run(["ssh-agent", "-a", str(sock)], capture_output=True)
            os.environ["SSH_AUTH_SOCK"] = str(sock)
            self.log(f"started the shared ssh-agent ({sock})")
        self.log("SSH key is locked: asking for its passphrase")
        env = dict(os.environ, SSH_ASKPASS=str(Path(__file__).resolve()),
                   SSH_ASKPASS_REQUIRE="force", WINSTREAM_ASKPASS="1")
        # The key by path: ssh-add's defaults use the home folder from the user
        # database, not $HOME. A wrong passphrase asks again; Cancel stops.
        r = subprocess.run(["ssh-add", str(key)], env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True)
        if self.key_loaded(key):
            self.log("SSH key unlocked")
            return True
        self.log("SSH key still locked (cancelled or wrong passphrase); nothing checked. "
                 + (r.stderr.strip().splitlines() or [""])[-1])
        return False

    @staticmethod
    def key_loaded(key):
        """True if the agent holds this key. Not just "any key": the agent
        may hold others that the host refuses."""
        r = subprocess.run(["ssh-keygen", "-lf", f"{key}.pub"], capture_output=True, text=True)
        if r.returncode != 0 or not r.stdout.split()[1:2]:
            # No .pub to compare with: fall back to "the agent holds a key".
            return subprocess.run(["ssh-add", "-l"], capture_output=True).returncode == 0
        loaded = subprocess.run(["ssh-add", "-l"], capture_output=True, text=True).stdout
        return r.stdout.split()[1] in loaded

    def pump(self):
        try:
            while True:
                kind, value = self.events.get_nowait()
                if kind == "log":
                    self.log_box.configure(state="normal")
                    self.log_box.insert("end", value + "\n")
                    self.log_box.see("end")
                    self.log_box.configure(state="disabled")
                elif kind == "status":
                    stage, text = value
                    stage.status.set(text)
                elif kind == "done":
                    self.busy = False
                    self.set_buttons("normal")
        except queue.Empty:
            pass
        self.root.after(50, self.pump)

    def toggle_overlay(self):
        """The client looks for this file twice a second, so running windows
        follow the box too."""
        if self.show_overlay.get():
            OVERLAY_OFF_FLAG.unlink(missing_ok=True)
        else:
            OVERLAY_OFF_FLAG.touch()

    def set_buttons(self, state):
        for stage in self.stages + self.app_stages:
            stage.button.configure(state=state)
        for button in self.app_buttons:
            button.configure(state=state)
        self.session_button.configure(state=state)

    # --- Apps column ------------------------------------------------------

    def build_apps(self):
        """(Re)draw the apps column from self.apps. Only while not busy, so
        no task holds a stage that goes away."""
        for child in self.app_frame.winfo_children():
            child.destroy()
        self.app_stages, self.app_buttons = [], []
        for i, app in enumerate(self.apps):
            stage = Stage(f"Start {app['name']}",
                          lambda a=dict(app): self.check_app(app_check(a)),
                          lambda a=dict(app): self.start_app(app_launch_command(a), app_check(a)))
            stage.button = ttk.Button(self.app_frame, text=stage.name, width=18,
                                      command=lambda s=stage: self.start(self.run_stage, s))
            stage.button.grid(row=i, column=0, sticky="w", pady=2)
            stage.status = tk.StringVar(value="")
            ttk.Label(self.app_frame, textvariable=stage.status, width=10).grid(row=i, column=1, sticky="w", padx=8)
            edit = ttk.Button(self.app_frame, text="✎", width=3, command=lambda n=i: self.edit_app(n))
            edit.grid(row=i, column=2, sticky="w")
            self.app_stages.append(stage)
            self.app_buttons.append(edit)
        add = ttk.Button(self.app_frame, text="+ Add app", command=lambda: self.edit_app(None))
        add.grid(row=len(self.apps), column=0, sticky="w", pady=(8, 2))
        self.app_buttons.append(add)

    def edit_app(self, index):
        if self.busy:
            return
        app = self.apps[index] if index is not None else \
            {"name": "", "dir": "", "command": "", "check": "", "start_with_session": False}
        AppDialog(self.root, app, index is not None, lambda result: self.app_edited(index, result))

    def app_edited(self, index, result):
        """result: the edited app, or None to delete it."""
        if result is None:
            del self.apps[index]
        elif index is None:
            self.apps.append(result)
        else:
            self.apps[index] = result
        save_apps(self.apps)
        self.build_apps()
        if result is not None:
            stage = self.app_stages[index if index is not None else len(self.apps) - 1]
            self.start(self.check_stage, stage)

    def log(self, text):
        self.events.put(("log", text))

    def set_status(self, stage, text):
        self.events.put(("status", (stage, text)))

    # --- Stages -------------------------------------------------------------

    def check_stage(self, stage):
        self.set_status(stage, "checking…")
        ok = stage.check()
        self.set_status(stage, "OK" if ok else "not done")
        return ok

    def run_stage(self, stage):
        self.set_status(stage, "running…")
        self.log(f"== {stage.name}")
        if not stage.action():
            self.set_status(stage, "failed")
            return False
        return self.check_stage(stage)

    def check_all(self):
        """Check the setup stages in order and stop at the first that isn't OK:
        the later ones depend on it, so checking them only produces failures
        (shown as "–", not checked). The apps are only checked once setup is
        all OK, and each on its own."""
        for i, stage in enumerate(self.stages):
            if not self.check_stage(stage):
                for later in self.stages[i + 1:] + self.app_stages:
                    self.set_status(later, "–")
                return
        for stage in self.app_stages:
            self.check_stage(stage)

    def start_session(self):
        # Every stage, then the apps ticked "Start with the session".
        apps = [s for s, app in zip(self.app_stages, self.apps) if app.get("start_with_session")]
        for stage in self.stages + apps:
            if self.check_stage(stage):
                continue
            if not self.run_stage(stage):
                self.log(f"stopped: {stage.name} failed")
                return

    def sh(self, cmd, **kw):
        """Run a command, logging its output; True if it succeeded."""
        self.log("$ " + " ".join(shlex.quote(c) for c in cmd))
        proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                text=True, cwd=HERE, **kw)
        for line in proc.stdout:
            self.log(ANSI.sub("", line.rstrip()))
        return proc.wait() == 0

    def quiet(self, cmd):
        """Run a command silently; (ok, stdout)."""
        r = subprocess.run(cmd, capture_output=True, text=True, cwd=HERE)
        return r.returncode == 0, r.stdout.strip()

    # 1. Wake
    def check_awake(self):
        return self.quiet(SSH + [HOST, "true"])[0]

    def wake(self):
        """host.wake, then wait for ssh to answer (a wake command may return
        before the host's network is up)."""
        if not CFG.wake:
            self.log(f"`ssh {HOST}` doesn't answer, and host.wake is not set in {CFG.path}")
            return False
        if not self.sh(["sh", "-c", CFG.wake]):
            return False
        for _ in range(24):
            if self.check_awake():
                return True
            threading.Event().wait(1)
        self.log(f"the wake command succeeded but `ssh {HOST}` doesn't answer after ~2 min")
        return False

    # 2. Home folder mount
    def check_mounted(self):
        # /proc/mounts, not the mount itself: touching a dead sshfs mount blocks.
        with open("/proc/mounts") as f:
            return any(line.split()[1:3] == [str(MOUNT), "fuse.sshfs"] for line in f)

    def mount(self):
        MOUNT.mkdir(parents=True, exist_ok=True)
        # Fail fast instead of hanging if the host goes away.
        return self.sh(["sshfs", f"{HOST}:", str(MOUNT), "-o",
                        "reconnect,ServerAliveInterval=10,ServerAliveCountMax=2,ConnectTimeout=5"])

    # 3. Server build (on the host)
    def check_server_built(self):
        ok, out = self.quiet(SSH + [HOST, f"cat {REMOTE_DIR}/{SERVER_STAMP} 2>/dev/null"])
        return ok and out == sources_hash(SERVER_SOURCES)

    def build_server(self):
        stamp = sources_hash(SERVER_SOURCES)
        return self.sh([str(HERE / "dev.sh"),
                        f"cargo build --release -p server 2>&1 && echo {stamp} > {SERVER_STAMP}"],
                       env=dict(os.environ, WINSTREAM_CONFIG=str(CFG.path)))

    # 4. Client build (here)
    def check_client_built(self):
        return CLIENT_STAMP.exists() and CLIENT_STAMP.read_text().strip() == sources_hash(CLIENT_SOURCES)

    def build_client(self):
        stamp = sources_hash(CLIENT_SOURCES)
        if not self.sh(["cargo", "build", "--release", "-p", "client"]):
            return False
        CLIENT_STAMP.write_text(stamp + "\n")
        return True

    # 5. Server: your command, in its own terminal.
    def check_server_running(self):
        # Listening on the port, not just "a process exists": a server whose
        # network died still shows up in pgrep but no client can connect.
        return self.quiet(SSH + [HOST, f"ss -Huln 'sport = :{PORT}' | grep -q ."])[0]

    def start_server(self):
        # By process name (-x): a -f pattern would also match the shell that
        # ssh starts for this very command, so it always "found" a server.
        if self.quiet(SSH + [HOST, "pgrep -x server >/dev/null"])[0]:
            self.log(f"a server is already running on {HOST} but not listening on port {PORT} "
                     f"(broken). Stop it first: ssh {HOST} pkill -TERM -x server")
            return False
        listen = f" --listen 0.0.0.0:{PORT}" if PORT != winstream_config.DEFAULT_PORT else ""
        listen += "".join(f" {shlex.quote(a)}" for a in authority_args())
        settings = (f" --fps {CFG.fps} --latency-ms {CFG.latency_ms} --start-bpp {CFG.start_bpp}"
                    f" --max-bpp {CFG.max_bpp} --min-mbit {CFG.min_mbit}")
        command = shlex.join(["ssh", HOST, f"cd {REMOTE_DIR} && ./target/release/server stream{listen}{settings}"])
        self.server_proc = self.terminal("server", command)
        # Up once it listens (it opens the port right away; the app follows).
        for _ in range(60):
            if self.server_proc.poll() is not None:
                self.log("the server exited; see its output above")
                return False
            if self.check_server_running():
                return True
        self.log("server isn't listening after ~30 s; see its output above")
        return False

    # 6. Client: your command, in its own terminal.
    def check_client_running(self):
        # Anchored: only the client itself, not the terminal or shell whose
        # command lines also contain this text.
        return self.quiet(["pgrep", "-f", client_pattern()])[0]

    def open_client(self):
        self.client_proc = self.terminal(
            "client", f"cd {shlex.quote(str(HERE))} && ./target/release/client {client_args()}")
        for _ in range(20):
            if self.check_client_running():
                return True
            threading.Event().wait(0.25)
        self.log("client didn't start; see its output above")
        return False

    # 7. Apps: started in the running server's session.
    def check_app(self, find):
        return self.quiet(SSH + [HOST, f"pgrep {find} >/dev/null"])[0]

    def start_app(self, command, find):
        if not self.sh(SSH + [HOST, f"{REMOTE_DIR}/target/release/server launch -- {command}"]):
            return False
        for _ in range(40):
            if self.check_app(find):
                return True
        self.log(f"{command} isn't running ~20 s after launch; see the server output")
        return False

    def terminal(self, title, command):
        """Run `command` in the background, its output going to a log file
        and shown in the log pane. With "Show terminals" ticked, open a
        terminal for it instead; it stays open after the command ends, so
        errors can be read (--disable-server: its own process, so Stop can
        close it)."""
        self.log(f"$ {command}")
        if self.show_terminals.get():
            terminal = [part.replace("{title}", f"winstream {title}") for part in CFG.terminal]
            return subprocess.Popen(
                terminal + ["bash", "-c", command],
                cwd=HERE, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            )
        # A file, not a pipe: if the launcher closes, a pipe would break and
        # take the command down with it. Own session: not tied to ours.
        LOG_DIR.mkdir(parents=True, exist_ok=True)
        path = LOG_DIR / f"launcher-{title}.log"
        with open(path, "w") as out:
            proc = subprocess.Popen(["bash", "-c", command], cwd=HERE, stdin=subprocess.DEVNULL,
                                    stdout=out, stderr=subprocess.STDOUT, start_new_session=True)
        self.log(f"   (output: {path})")

        def follow():
            with open(path) as f:
                while True:
                    line = f.readline()
                    if line:
                        self.log(f"{title}: " + ANSI.sub("", line.rstrip()))
                    elif proc.poll() is not None:
                        self.log(f"{title} exited ({proc.returncode})")
                        return
                    else:
                        threading.Event().wait(0.2)

        threading.Thread(target=follow, daemon=True).start()
        return proc

    # --- Stop ------------------------------------------------------------

    def watch_processes(self):
        """Mark server/client "not done" when their terminal is closed."""
        if not self.busy:
            for proc, stage in ((self.server_proc, self.server_stage), (self.client_proc, self.client_stage)):
                if proc and proc.poll() is not None and stage.status.get() == "OK":
                    stage.status.set("not done")
        self.root.after(1000, self.watch_processes)

    def stop(self):
        self.start(self.stop_all)

    def stop_all(self):
        """Stop in order: the apps started in the session (and everything they
        spawned; 30 s to save, then killed), then the server, then the
        client and the terminals. Also works for a server or client this
        launcher didn't start."""
        self.log("== Stop")
        if self.quiet(SSH + [HOST, "pgrep -x server >/dev/null"])[0]:
            self.sh(SSH + [HOST, f"{REMOTE_DIR}/target/release/server stop-apps"])
            self.sh(SSH + [HOST,
                "pkill -TERM -x server; "
                "for i in $(seq 50); do pgrep -x server >/dev/null || { echo server stopped; exit 0; }; sleep 0.2; done; "
                "echo server still running after 10 s, killing it; pkill -KILL -x server"])
        else:
            self.log(f"no server running on {HOST}")
        if self.quiet(["pkill", "-TERM", "-f", client_pattern()])[0]:
            self.log("client stopped")
        for proc in (self.client_proc, self.server_proc):
            if proc and proc.poll() is None:
                proc.terminate()
        self.client_proc = self.server_proc = None
        self.check_all()

    def quit(self):
        # The terminals stay open; close them (or press Stop) to end the session.
        self.root.destroy()


class AppDialog(tk.Toplevel):
    """Add or edit one app. on_done(app) on Save, on_done(None) on Delete."""

    def __init__(self, parent, app, existing, on_done):
        super().__init__(parent)
        self.title("Edit app" if existing else "Add app")
        self.transient(parent)
        self.on_done = on_done
        body = ttk.Frame(self, padding=10)
        body.pack(fill="both", expand=True)
        self.vars = {}
        fields = [
            ("name", "Name"),
            ("dir", f"Folder on {HOST}"),
            ("command", "Command"),
            ("check", "Running check\n(pgrep args, optional)"),
        ]
        for row, (key, label) in enumerate(fields):
            ttk.Label(body, text=label).grid(row=row, column=0, sticky="w", padx=(0, 8), pady=3)
            self.vars[key] = tk.StringVar(value=app.get(key, ""))
            ttk.Entry(body, textvariable=self.vars[key], width=48).grid(row=row, column=1, sticky="we", pady=3)
        ttk.Button(body, text="Browse…", command=self.browse).grid(row=1, column=2, padx=(4, 0))
        self.start_with_session = tk.BooleanVar(value=bool(app.get("start_with_session")))
        ttk.Checkbutton(body, text="Start with the session", variable=self.start_with_session).grid(
            row=len(fields), column=1, sticky="w", pady=3)
        ttk.Label(body, foreground="#666", justify="left", text=(
            f"Runs on {HOST} as: cd FOLDER && COMMAND, inside the server's session.\n"
            "Running check empty: looks for a process named like the command's program.")
        ).grid(row=len(fields) + 1, column=0, columnspan=3, sticky="w", pady=(6, 0))
        buttons = ttk.Frame(body)
        buttons.grid(row=len(fields) + 2, column=0, columnspan=3, sticky="e", pady=(10, 0))
        if existing:
            ttk.Button(buttons, text="Delete", command=self.delete).pack(side="left", padx=(0, 16))
        ttk.Button(buttons, text="Cancel", command=self.destroy).pack(side="left", padx=(0, 4))
        ttk.Button(buttons, text="Save", command=self.save).pack(side="left")
        body.columnconfigure(1, weight=1)
        self.bind("<Return>", lambda _e: self.save())
        self.bind("<Escape>", lambda _e: self.destroy())
        self.grab_set()

    def browse(self):
        """Pick the folder through the sshfs mount; stored as a path on the host."""
        if MOUNT is None:
            messagebox.showinfo("Browse", "No mount.path in the configuration: type the folder.", parent=self)
            return
        if not MOUNT.is_dir() or not any(MOUNT.iterdir()):
            messagebox.showinfo("Browse", f"{MOUNT} isn't mounted: press \"Mount home\" first, "
                                          "or type the folder.", parent=self)
            return
        current = self.vars["dir"].get().strip()
        start = MOUNT / current[2:] if current.startswith("~/") else MOUNT
        chosen = filedialog.askdirectory(parent=self, initialdir=start if start.is_dir() else MOUNT,
                                         mustexist=True)
        if not chosen:
            return
        path = Path(chosen)
        if path == MOUNT or MOUNT in path.parents:
            rel = path.relative_to(MOUNT)
            self.vars["dir"].set("~" if str(rel) == "." else f"~/{rel}")
        else:
            messagebox.showinfo("Browse", f"Pick a folder inside {MOUNT} ({HOST}'s home).", parent=self)

    def save(self):
        app = {key: var.get().strip() for key, var in self.vars.items()}
        app["start_with_session"] = self.start_with_session.get()
        if not app["name"] or not app["command"]:
            messagebox.showinfo("App", "Name and command are needed.", parent=self)
            return
        try:
            shlex.split(app["command"]), shlex.split(app["check"])
        except ValueError as err:
            messagebox.showinfo("App", f"Can't read that: {err}", parent=self)
            return
        self.destroy()
        self.on_done(app)

    def delete(self):
        self.destroy()
        self.on_done(None)


def main():
    if os.environ.get("WINSTREAM_ASKPASS"):
        sys.exit(askpass(sys.argv[1] if len(sys.argv) > 1 else "Passphrase:"))
    cfg = load_config()
    if cfg is None:
        sys.exit(1)
    configure(cfg)
    root = tk.Tk(className="winstream-launcher")
    Launcher(root)
    root.mainloop()


if __name__ == "__main__":
    main()
