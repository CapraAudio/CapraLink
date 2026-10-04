import asyncio
import glob
import json
import os

import decky

TIMEOUT = 10  # seconds per CLI call


def _appimage():
    found = sorted(glob.glob(os.path.join(decky.DECKY_USER_HOME, "Applications", "CapraLink", "*.AppImage")))
    return found[-1] if found else None


async def _run(*args):
    """Runs `capralink <args>` as the Deck user; returns {"ok": bool, "data": parsed JSON or None, "error": str}."""
    exe = _appimage()
    if not exe:
        return {"ok": False, "data": None, "error": "CapraLink isn't installed. Install it in Desktop Mode first."}
    cmd = [exe, *args]
    if os.geteuid() == 0:  # in case Decky runs the plugin as root: the engine belongs to the Deck user
        cmd = ["runuser", "-u", decky.DECKY_USER, "--", *cmd]
    env = {**os.environ, "HOME": decky.DECKY_USER_HOME}
    try:
        p = await asyncio.create_subprocess_exec(*cmd, env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        out, err = await asyncio.wait_for(p.communicate(), TIMEOUT)
    except asyncio.TimeoutError:
        p.kill()
        return {"ok": False, "data": None, "error": "CapraLink didn't answer in time."}
    except OSError as e:
        return {"ok": False, "data": None, "error": f"Can't run CapraLink: {e}"}
    if p.returncode != 0:
        msg = err.decode(errors="replace").strip() or f"exit code {p.returncode}"
        return {"ok": False, "data": None, "error": msg.removeprefix("capralink: ")}
    try:
        data = json.loads(out) if out.strip() else None
    except ValueError:
        return {"ok": False, "data": None, "error": "Unexpected reply from CapraLink."}
    return {"ok": True, "data": data, "error": ""}


class Plugin:
    async def status(self):
        return await _run("--status")

    async def connect(self, device: str):
        return await _run("--connect", device)

    async def disconnect(self):
        return await _run("--disconnect")

    async def music(self, on: bool):
        return await _run("--music", "on" if on else "off")
