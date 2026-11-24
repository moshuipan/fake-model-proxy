"""用本机程序验证含中文、空格和百分号的数据路径，不连接真实后端。"""

import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


def main():
    """启动隔离实例，检查嵌入页面、健康接口和数据落盘后退出。"""
    executable = Path(__file__).resolve().parents[2] / "target/debug/fakemodelproxy"
    with tempfile.TemporaryDirectory(prefix="fmp-windows-smoke-") as root:
        data = Path(root) / "中文 空格 100%25" / "data"
        with socket.socket() as socket_handle:
            socket_handle.bind(("127.0.0.1", 0))
            port = socket_handle.getsockname()[1]
        env = {key: value for key, value in os.environ.items() if not key.startswith("FMP_")}
        env.update(FMP_LISTEN=f"127.0.0.1:{port}", FMP_DATA_DIR=str(data))
        base = f"http://127.0.0.1:{port}"
        with (Path(root) / "server.log").open("w+") as log:
            process = subprocess.Popen([str(executable)], cwd=root, env=env, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 30
                while True:
                    if process.poll() is not None:
                        log.seek(0)
                        raise RuntimeError(log.read())
                    try:
                        with urllib.request.urlopen(base + "/health", timeout=1) as response:
                            assert response.status == 200
                            health = json.load(response)
                            assert isinstance(health, dict) and health.get("ok") is True, health
                        break
                    except (urllib.error.URLError, TimeoutError):
                        if time.monotonic() >= deadline:
                            log.seek(0)
                            raise RuntimeError(log.read())
                        time.sleep(0.1)
                for endpoint, expected_type in [
                    ("/", "text/html"),
                    ("/assets/console.js", "javascript"),
                    ("/assets/console.css", "text/css"),
                    ("/v1/models", "application/json"),
                ]:
                    with urllib.request.urlopen(base + endpoint, timeout=5) as response:
                        body = response.read()
                        assert response.status == 200 and body
                        assert expected_type in response.headers["Content-Type"]
                assert (data / "fakemodelproxy.db").is_file()
                print("PASS: health, embedded UI, models, Chinese/space/percent database path")
            finally:
                if process.poll() is None:
                    process.send_signal(signal.SIGINT)
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
                if process.returncode not in (0, None):
                    log.seek(0)
                    print(log.read())
            assert process.returncode == 0


if __name__ == "__main__":
    main()
