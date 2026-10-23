#!/usr/bin/env python3
"""Process smoke: real dashboard + background services, no network trading."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def main():
    binary = str(Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory(prefix="girasol-v3-smoke-") as directory:
        root = Path(directory)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        env = dict(os.environ, SNIPER_DATA_DIR=str(root / "data"),
                   SNIPER_PANEL_BIND=f"127.0.0.1:{port}",
                   PANEL_AUTH_TOKEN="isolated-smoke-token-no-production-secret")
        # Avoid inheriting operator credentials into this isolated process.
        for key in list(env):
            if any(word in key for word in ["TELEGRAM", "PRIVATE_KEY", "WALLET_KEY", "LLM_"]):
                env.pop(key)
        log = open(root / "process.log", "w")
        bot_args = [] if "--default-startup" in sys.argv[2:] else ["--panel-only", "--evm-config", str(root / "must-not-load.toml")]
        process = subprocess.Popen([binary] + bot_args, cwd=root, env=env,
                                   stdout=log, stderr=subprocess.STDOUT)
        base = f"http://127.0.0.1:{port}"

        def request(path, body=None, authorized=True):
            headers = {}
            if authorized:
                headers["Authorization"] = "Bearer " + env["PANEL_AUTH_TOKEN"]
            data = None if body is None else json.dumps(body).encode()
            if data is not None:
                headers["Content-Type"] = "application/json"
            try:
                with urllib.request.urlopen(urllib.request.Request(base + path, data=data, headers=headers), timeout=10) as response:
                    return response.status, response.read()
            except urllib.error.HTTPError as error:
                return error.code, error.read()

        try:
            deadline = time.monotonic() + 120
            while True:
                if process.poll() is not None:
                    raise RuntimeError((root / "process.log").read_text())
                try:
                    if request("/", authorized=False)[0] == 200:
                        break
                except OSError:
                    pass
                if time.monotonic() >= deadline:
                    raise TimeoutError("dashboard startup timeout")
                time.sleep(0.25)
            assert request("/api/panel/status", authorized=False)[0] == 401
            status, payload = request("/api/panel/status")
            assert status == 200 and json.loads(payload)["live_enabled"] is False
            for path in ["keys", "slots", "positions", "history", "pnl", "logs"]:
                assert request("/api/panel/" + path)[0] == 200, path
            status, _ = request("/api/panel/live", {
                "enabled": True, "confirmation": "I UNDERSTAND REAL FUNDS ARE AT RISK"})
            assert status in [400, 403], "panel must not enable live without --real"
            assert request("/api/panel/status")[0] == 200
            assert process.poll() is None, "background services must not block or stop panel"
            print(json.dumps({"process_smoke": "PASS", "public_html": True,
                              "api_auth": True, "live_without_cli_rejected": True,
                              "submitted_transactions": 0}))
        finally:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            log.close()


if __name__ == "__main__":
    main()
