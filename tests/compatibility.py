"""Run the pinned official ACPv2 TCK against Bee and a local deterministic model.

Usage: uv run --no-project python tests/compatibility.py target/debug/bee [TCK options]
"""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


# Pin the draft suite as carefully as the Rust SDK: upstream main can change
# its schema and mandatory requirements independently of Bee's implementation.
TCK_REVISION = "b7f40fb3c003c45926547be5a1f7e2b374035815"
CANCEL_PROMPT = "BEE_TCK_CANCEL_PENDING_MODEL_REQUEST"


class Model(BaseHTTPRequestHandler):
    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if CANCEL_PROMPT in json.dumps(request["messages"][-1]):
            # Give the TCK a pending HTTP operation to cancel, without relying
            # on inference speed or any real credentials/provider availability.
            threading.Event().wait(10)
        body = json.dumps(
            {
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "Fixture answer."},
                        "finish_reason": "stop",
                    }
                ]
            }
        ).encode()
        try:
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            # A cancelled model request intentionally closes its HTTP connection.
            pass

    def log_message(self, *_args):
        pass


def main():
    binary = Path(sys.argv[1]).resolve()
    if os.name == "nt":
        binary = binary.with_suffix(".exe")
    if not binary.is_file():
        raise SystemExit(f"Build Bee first: cargo build (missing {binary})")
    model = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    thread = threading.Thread(target=model.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="bee-acp-tck-") as temporary:
            root = Path(temporary)
            config = root / "bee.toml"
            config.write_text(
                "model='test/fixture'\n[accounts.test]\nprovider='custom'\nmodels=['fixture']\n"
                "[accounts.test.protocols.openai-chat]\n"
                f"base_url='http://127.0.0.1:{model.server_port}'\n",
                encoding="utf-8",
            )
            command = [
                "uvx", "--from",
                f"git+https://github.com/agentclientprotocol/acp-tck@{TCK_REVISION}",
                "acp-tck", "--protocol-version", "2",
                "--agent-cwd", str(root),
                "--agent-env", f"HOME={root}",
                "--agent-env", f"XDG_CONFIG_HOME={root / 'xdg'}",
                "--agent-env", f"BEE_CONFIG={config}",
                "--cancel-prompt", CANCEL_PROMPT,
                "--timeout", "5", "--test-timeout", "30",
                *sys.argv[2:], "--", str(binary),
            ]
            return subprocess.run(command, check=False).returncode
    finally:
        model.shutdown()
        model.server_close()
        thread.join()


if __name__ == "__main__":
    raise SystemExit(main())
