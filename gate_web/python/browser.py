"""Manages Xvfb + Chrome + x11vnc + noVNC for browser-based Gate.io access."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path


_X11VNC_LIB = "/tmp/x11vnc-extract/usr/lib/x86_64-linux-gnu"
_X11VNC_BIN = "/tmp/x11vnc-extract/usr/bin/x11vnc"
_NOVNC_DIR = Path.home() / ".local/share/noVNC"


@dataclass
class BrowserManager:
    """Launches a headless Chrome behind VNC so the user can log in to Gate.io."""

    display: int = 11
    vnc_port: int = 5911
    novnc_port: int = 6091
    chrome_debug_port: int = 9231
    chrome_user_data: str = "/tmp/chrome-gate"
    start_url: str = "https://www.gate.com/login"

    _procs: list[subprocess.Popen] = field(default_factory=list, repr=False)
    _running: bool = field(default=False, repr=False)

    def start(self) -> str:
        """Start all services. Returns the noVNC URL."""
        if self._running:
            return self.novnc_url

        self._ensure_deps()
        self._start_xvfb()
        self._start_chrome()
        self._start_x11vnc()
        self._start_novnc()
        self._running = True
        return self.novnc_url

    def stop(self):
        """Kill all child processes."""
        for p in reversed(self._procs):
            try:
                p.terminate()
                p.wait(timeout=5)
            except Exception:
                p.kill()
        self._procs.clear()
        self._running = False

    @property
    def novnc_url(self) -> str:
        return f"http://0.0.0.0:{self.novnc_port}/vnc.html"

    @property
    def cdp_ws_url(self) -> str:
        """Chrome DevTools Protocol WebSocket URL."""
        import urllib.request

        resp = urllib.request.urlopen(
            f"http://localhost:{self.chrome_debug_port}/json/version"
        )
        data = json.loads(resp.read())
        return data["webSocketDebuggerUrl"]

    def _ensure_deps(self):
        if not Path(_X11VNC_BIN).exists():
            raise RuntimeError(
                "x11vnc not found. Run:\n"
                "  cd /tmp && apt download x11vnc libvncserver1 libvncclient1\n"
                "  dpkg-deb -x x11vnc_*.deb x11vnc-extract\n"
                "  dpkg-deb -x libvncserver1_*.deb x11vnc-extract\n"
                "  dpkg-deb -x libvncclient1_*.deb x11vnc-extract"
            )
        if not _NOVNC_DIR.exists():
            raise RuntimeError(
                "noVNC not found. Run:\n"
                "  git clone --depth 1 https://github.com/novnc/noVNC.git "
                f"{_NOVNC_DIR}"
            )
        if not shutil.which("Xvfb"):
            raise RuntimeError("Xvfb not installed (apt install xvfb)")
        if not shutil.which("google-chrome"):
            raise RuntimeError("google-chrome not found")

    def _start_xvfb(self):
        p = subprocess.Popen(
            ["Xvfb", f":{self.display}", "-screen", "0", "1920x1080x24"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self._procs.append(p)
        time.sleep(0.5)

    def _start_chrome(self):
        env = os.environ.copy()
        env["DISPLAY"] = f":{self.display}"
        p = subprocess.Popen(
            [
                "google-chrome",
                "--no-sandbox",
                "--disable-gpu",
                f"--remote-debugging-port={self.chrome_debug_port}",
                "--remote-debugging-address=0.0.0.0",
                f"--user-data-dir={self.chrome_user_data}",
                "--window-size=1920,1080",
                self.start_url,
            ],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self._procs.append(p)
        time.sleep(2)

    def _start_x11vnc(self):
        env = os.environ.copy()
        env["LD_LIBRARY_PATH"] = _X11VNC_LIB
        p = subprocess.Popen(
            [
                _X11VNC_BIN,
                "-display",
                f":{self.display}",
                "-nopw",
                "-forever",
                "-shared",
                "-rfbport",
                str(self.vnc_port),
            ],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self._procs.append(p)
        time.sleep(1)

    def _start_novnc(self):
        proxy_script = _NOVNC_DIR / "utils" / "novnc_proxy"
        p = subprocess.Popen(
            [
                str(proxy_script),
                "--vnc",
                f"localhost:{self.vnc_port}",
                "--listen",
                str(self.novnc_port),
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self._procs.append(p)
        time.sleep(1)
