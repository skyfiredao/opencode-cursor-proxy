import argparse
import json
import os
from pathlib import Path
import shlex
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def main():
    parser = argparse.ArgumentParser(
        description="Exercise the release binary with recorded fixtures, without a Cursor login."
    )
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    fixture = Path(__file__).resolve().parents[1] / "tests/fixtures/text.ndjson"
    password = "fixture-smoke-password"
    with tempfile.TemporaryDirectory(prefix="cursor-proxy-smoke-") as temporary:
        workspace = Path(temporary)
        (workspace / "AGENTS.md").write_text("SMOKE_WORKSPACE_RULE", encoding="utf-8")
        directory = workspace / "config/opencode"
        directory.mkdir(parents=True)
        config_path = directory / "opencode.jsonc"
        original = '{\n// preserve this comment\n"model": "other/current"\n}\n'
        config_path.write_text(original, encoding="utf-8")
        environment = dict(os.environ, XDG_CONFIG_HOME=str(workspace / "config"))
        environment.pop("OPENCODE_CONFIG_DIR", None)
        opencode = workspace / "opencode"
        opencode.write_text("#!/bin/sh\necho 2.2.7\n", encoding="utf-8")
        opencode.chmod(0o700)
        environment["PATH"] = str(workspace) + os.pathsep + environment.get("PATH", "")
        legacy = workspace / "recorded-cursor-agent"
        legacy.write_text(
            "#!/bin/sh\n"
            'if [ "$1" = "--list-models" ]; then\n'
            f"  printf 'refresh\\n' >> {shlex.quote(str(workspace / 'model-discoveries'))}\n"
            "  printf 'Available models\\ncomposer-2.5 - Composer 2.5\\n'\n"
            "  exit 0\n"
            "fi\n"
            "prompt=$(cat)\n"
            "case \"$prompt\" in\n"
            "  *'SMOKE_WORKSPACE_RULE'*) ;;\n"
            "  *) echo 'missing workspace instructions' >&2; exit 1 ;;\n"
            "esac\n"
            f"cat {shlex.quote(str(fixture))}\n",
            encoding="utf-8",
        )
        legacy.chmod(0o700)
        agent = workspace / "mock-cursor-agent"
        adapter = Path(__file__).resolve().parents[1] / "tests/fixtures/acp-agent.py"
        agent.write_text(
            "#!/bin/sh\ncase \" $* \" in\n"
            f"  *' acp '*) exec {shlex.quote(sys.executable)} {shlex.quote(str(adapter))} --legacy {shlex.quote(str(legacy))} \"$@\" ;;\n"
            f"  *) exec {shlex.quote(str(legacy))} \"$@\" ;;\nesac\n",
            encoding="utf-8",
        )
        agent.chmod(0o700)
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        base_url = f"http://127.0.0.1:{port}"
        with (workspace / "server.log").open("w+") as log:
            process = subprocess.Popen(
                [str(binary), "--host", "127.0.0.1", "--port", str(port),
                 "--workspace", str(workspace), "--cursor-agent", str(agent),
                 "--password", password, "--load-skills"],
                stdout=log,
                stderr=log,
                env=environment,
            )

            def request(path, body=None, authenticated=True):
                headers = {"Content-Type": "application/json"}
                if authenticated:
                    headers["Authorization"] = f"Bearer {password}"
                data = None if body is None else json.dumps(body).encode()
                req = urllib.request.Request(base_url + path, data=data, headers=headers)
                with urllib.request.urlopen(req, timeout=5) as response:
                    return response.read().decode()

            try:
                deadline = time.monotonic() + 10
                while True:
                    if process.poll() is not None:
                        raise RuntimeError(f"Proxy exited during startup: {process.returncode}")
                    try:
                        health = json.loads(request("/health", authenticated=False))
                        assert health["ok"] is True
                        break
                    except (urllib.error.URLError, ConnectionError):
                        if time.monotonic() >= deadline:
                            raise TimeoutError("Proxy did not become ready")
                        time.sleep(0.05)
                print("PASS health")
                try:
                    request("/v1/models", authenticated=False)
                except urllib.error.HTTPError as error:
                    assert error.code == 401, error.code
                else:
                    raise AssertionError("Unauthenticated request was accepted")
                print("PASS access password")
                while True:
                    models = json.loads(request("/v1/models"))
                    if [model["id"] for model in models["data"]] == ["composer-2.5"]:
                        break
                    if time.monotonic() >= deadline:
                        raise TimeoutError("Startup model refresh did not publish its catalog")
                    time.sleep(0.05)
                merged = config_path.read_text(encoding="utf-8")
                assert "// preserve this comment" in merged, merged
                server_config = json.loads(merged.replace("// preserve this comment", ""))
                assert server_config["model"] == "other/current", server_config
                assert "composer-2.5" in server_config["providers"]["cursor-proxy"]["models"], server_config
                assert server_config["providers"]["cursor-proxy"]["settings"]["baseURL"] == base_url + "/v1", server_config
                assert server_config["providers"]["cursor-proxy"]["settings"]["apiKey"] == password, server_config
                assert server_config["providers"]["cursor-proxy"]["models"]["composer-2.5"]["capabilities"]["tools"] is True
                backups = list(directory.glob("opencode.jsonc.cursor-proxy-*.bak"))
                assert len(backups) == 1, backups
                assert backups[0].read_text(encoding="utf-8") == original
                for _ in range(3):
                    request("/v1/models")
                assert (workspace / "model-discoveries").read_text().splitlines() == ["refresh"]
                assert config_path.read_text(encoding="utf-8") == merged
                assert list(directory.glob("opencode.jsonc.cursor-proxy-*.bak")) == backups
                print("PASS automatic configuration, preserved settings, backup and cache-only model requests")
                body = {"model": "composer-2.5", "messages": [
                    {"role": "user", "content": "Reply with exactly: PONG"}
                ]}
                completion = json.loads(request("/v1/chat/completions", body))
                assert completion["choices"][0]["message"]["content"] == "PONG", completion
                assert "usage" not in completion, completion
                print("PASS non-stream ACP chat without fabricated usage")
                body["stream"] = True
                stream = request("/v1/chat/completions", body)
                payloads = [line[6:] for line in stream.splitlines() if line.startswith("data: ")]
                assert payloads[-1] == "[DONE]", stream
                chunks = [json.loads(payload) for payload in payloads[:-1]]
                text = "".join(choice.get("delta", {}).get("content", "")
                               for chunk in chunks for choice in chunk["choices"])
                assert text == "PONG", stream
                assert chunks[-1]["choices"][0]["finish_reason"] == "stop", stream
                print("PASS SSE chat, deduplication and final chunks")
                print("PASS forced AGENTS.md loading")
            except BaseException:
                log.flush()
                log.seek(0)
                print(log.read())
                raise
            finally:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            assert process.returncode == 0, process.returncode
            print("PASS graceful shutdown")


if __name__ == "__main__":
    main()
