#!/usr/bin/env python3
"""采集 TRAE CN 官方设备元数据，输出供 FMP 导入的 JSON。需要 Python 3.9+，无第三方依赖。"""

import argparse
import json
import os
from pathlib import Path
import platform
import re
import shlex
import subprocess
import sys

REPORT_VERSION = "fmp/client-device/v1"
MAX_FILE_BYTES = 16 * 1024 * 1024


def command(args):
    """执行固定的本地只读命令；失败不回显命令输出或环境信息。"""
    try:
        result = subprocess.run(args, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=5)
        return result.stdout.strip() if result.returncode == 0 else ""
    except (OSError, subprocess.SubprocessError, UnicodeError):
        return ""


def read_json(path):
    """读取有大小限制的 JSON；异常不回显可能包含凭证的文件内容。"""
    try:
        if path.stat().st_size > MAX_FILE_BYTES:
            return {}
        value = json.loads(path.read_text(encoding="utf-8"))
        return value if isinstance(value, dict) else {}
    except (OSError, ValueError, UnicodeError):
        return {}


def product_path(app):
    """兼容 product.json、resources/app、安装根目录和 macOS 应用包。"""
    app = Path(app).expanduser()
    choices = [app] if app.name == "product.json" else [
        app / "product.json", app / "resources/app/product.json",
        app / "Contents/Resources/app/product.json",
    ]
    return next((path for path in choices if path.is_file()), choices[-1])


def windows_system():
    """通过系统 CIM 读取硬件型号与 Node os.version 使用的 Windows 产品名。"""
    raw = command(["powershell", "-NoProfile", "-NonInteractive", "-Command",
                   "[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false);"
                   "$c=Get-CimInstance Win32_ComputerSystem;"
                   "$o=Get-CimInstance Win32_OperatingSystem;"
                   "@{model=$c.Model;caption=$o.Caption}|ConvertTo-Json -Compress"])
    try:
        value = json.loads(raw)
        return value if isinstance(value, dict) else {}
    except ValueError:
        return {}


def system_device():
    """按官方系统信息服务的平台规则读取型号和 OS 字符串，不填样例值。"""
    system = platform.system()
    if system == "Darwin":
        release = command(["sw_vers", "-productVersion"])
        return {"device_type": "mac", "device_brand": command(["sysctl", "-n", "hw.model"]),
                "os_version": "macOS " + release if release else ""}
    if system == "Windows":
        info = windows_system()
        caption = info.get("caption", "")
        caption = caption if isinstance(caption, str) else ""
        return {"device_type": "windows", "device_brand": info.get("model", ""),
                "os_version": caption.removeprefix("Microsoft ")}
    if system == "Linux":
        release = {}
        try:
            for line in Path("/etc/os-release").read_text(encoding="utf-8").splitlines():
                key, separator, value = line.partition("=")
                if separator:
                    parts = shlex.split(value)
                    release[key] = parts[0] if parts else ""
        except (OSError, ValueError, UnicodeError):
            pass
        try:
            model = Path("/sys/class/dmi/id/product_name").read_text(encoding="utf-8").strip()
        except (OSError, UnicodeError):
            model = ""
        return {"device_type": "linux", "device_brand": model,
                "os_version": release.get("PRETTY_NAME", "Linux")}
    return {"device_type": "", "device_brand": "", "os_version": ""}


def registered_device(profile):
    """只提取最新官方主进程日志的 ICDRS 注册结果；不采用备用 ID 或 URL 参数。"""
    try:
        logs = sorted((profile / "logs").glob("*/main.log"), reverse=True)[:20]
        for path in logs:
            with path.open("rb") as stream:
                # 注册信息在启动阶段；限制读取量，避免扫描聊天和请求日志。
                text = stream.read(MAX_FILE_BYTES).decode("utf-8", errors="replace")
            matches = re.findall(r"\[ICDRS\][^\r\n]*\brdid:\s*([1-9][0-9]{0,19})(?![0-9])", text)
            if matches:
                return matches[-1]
    except OSError:
        pass
    return ""


def default_pairs():
    """发现当前用户的标准 CN 安装和配置目录，保持安装与配置的对应关系。"""
    home = Path.home()
    system = platform.system()
    if system == "Darwin":
        profiles = [home / "Library/Application Support/Trae CN"]
        apps = [Path("/Applications/Trae CN.app"), home / "Applications/Trae CN.app"]
    elif system == "Windows":
        profiles = [Path(os.environ.get("APPDATA", str(home / "AppData/Roaming"))) / "Trae CN"]
        apps = [Path(os.environ.get("LOCALAPPDATA", str(home / "AppData/Local"))) / "Programs/Trae CN",
                Path(os.environ.get("ProgramFiles", "C:/Program Files")) / "Trae CN"]
    else:
        profiles = [Path(os.environ.get("XDG_CONFIG_HOME", str(home / ".config"))) / "Trae CN"]
        apps = [Path("/opt/trae-cn"), Path("/usr/share/trae-cn")]
    return [(profile, product_path(app)) for profile in profiles for app in apps
            if profile.is_dir() and product_path(app).is_file()]


def running_pairs():
    """发现运行中的 CN 主进程及 --user-data-dir，排除 Helper 与扩展子进程。"""
    if platform.system() == "Windows":
        raw = command(["powershell", "-NoProfile", "-NonInteractive", "-Command",
                       "[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false);"
                       "Get-CimInstance Win32_Process -Filter \"Name='Trae CN.exe'\" |"
                       "Select-Object ExecutablePath,CommandLine|ConvertTo-Json -Compress"])
        try:
            processes = json.loads(raw)
            processes = [processes] if isinstance(processes, dict) else processes
            rows = [(p.get("ExecutablePath", ""), p.get("CommandLine", ""))
                    for p in (processes or []) if isinstance(p, dict)]
        except ValueError:
            rows = []
    else:
        rows = []
        for line in command(["ps", "-x", "-o", "command="]).splitlines():
            match = re.match(r"(.+?/Trae CN\.app/Contents/MacOS/[^ ]+)(?: |$)", line)
            if not match and platform.system() == "Linux":
                match = re.match(r"(\S*/trae-cn)(?: |$)", line)
            if match:
                rows.append((match.group(1), line))
    pairs = []
    for executable, line in rows:
        if not isinstance(executable, str) or not isinstance(line, str) or "--type=" in line:
            continue
        match = re.search(r'--user-data-dir(?:=|\s+)("[^"]+"|\'[^\']+\'|\S+)', line)
        if not match:
            continue
        profile = Path(match.group(1).strip("\"'")).expanduser()
        app = Path(executable)
        if ".app/Contents/MacOS/" in executable:
            app = app.parents[2]
        else:
            app = app.parent
        product = product_path(app)
        if profile.is_dir() and product.is_file():
            pairs.append((profile, product))
    return pairs


def collect(pairs, system):
    """输出字段白名单；不同配置或安装生成独立候选，缺项单独说明。"""
    candidates, warnings, seen = [], [], set()
    for profile, product_file in pairs:
        key = (str(profile.resolve()), str(product_file.resolve()))
        if key in seen:
            continue
        seen.add(key)
        product = read_json(product_file)
        storage = read_json(profile / "User/globalStorage/storage.json")
        value = {**system, "device_id": registered_device(profile),
                 "app_version": product.get("appVersion", ""),
                 "ide_version": product.get("version", ""),
                 "machine_id": storage.get("telemetry.machineId", ""),
                 "plugin_version": product.get("tronBuildVersion") or "local",
                 "app_type": product.get("quality") or "stable"}
        label = str(profile)
        missing = [key for key in ("device_id", "device_brand", "device_type", "os_version", "app_version")
                   if not isinstance(value[key], str) or not value[key]]
        if missing:
            warnings.append(label + "：缺少 " + ", ".join(missing))
            continue
        # 不输出任何非字符串值，防止被修改的白名单键夹带其他存储对象。
        if any(not isinstance(item, str) for item in value.values()):
            warnings.append(label + "：设备元数据格式无效")
            continue
        if not value["machine_id"]:
            warnings.append(label + "：未读取到 telemetry.machineId；授权将使用 FMP 的服务机器标识。")
        if not value["ide_version"]:
            warnings.append(label + "：未读取到 product.version；推理请求不发送未知的 IDE 内核版本。")
        candidates.append({"label": label + " · " + value["app_version"], "value": value})
    if not pairs:
        warnings.append("未找到 TRAE CN 安装与用户目录；请先启动官方客户端，或指定 --profile 和 --app。")
    if not candidates:
        warnings.append("没有完整设备信息。请先启动官方客户端并联网完成设备注册；FMP 不注册或生成设备 ID。")
    return {"api_version": REPORT_VERSION, "plugin_id": "trae_personal",
            "candidates": candidates, "warnings": warnings}


def main():
    """解析显式路径与输出文件；stdout 只含白名单 JSON，失败不打印文件内容。"""
    if sys.version_info < (3, 9):
        sys.stderr.write("需要 Python 3.9 或更高版本。\n")
        return 2
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", help="官方客户端 user-data-dir")
    parser.add_argument("--app", help="官方客户端应用包、安装目录或 product.json")
    parser.add_argument("--output", help="保存 JSON 文件；省略时输出至 stdout")
    args = parser.parse_args()
    pairs = running_pairs() + default_pairs()
    if args.profile and args.app:
        pairs = [(Path(args.profile).expanduser(), product_path(args.app))]
    elif args.profile:
        pairs = [(Path(args.profile).expanduser(), product) for _, product in pairs]
    elif args.app:
        pairs = [(profile, product_path(args.app)) for profile, _ in pairs]
    report = collect(pairs, system_device())
    text = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    try:
        if args.output:
            Path(args.output).expanduser().write_text(text, encoding="utf-8")
        else:
            sys.stdout.write(text)
    except (OSError, UnicodeError):
        sys.stderr.write("无法写入设备信息 JSON，请检查输出路径权限。\n")
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
