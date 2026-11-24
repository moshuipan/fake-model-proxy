#!/usr/bin/env python3
"""从已检查的源码白名单生成确定性 ZIP；需要 Python 3.11+。"""

import argparse
import hashlib
from pathlib import Path
import re
import sys
import tempfile
import tomllib
import zipfile

from check_public_source import check_files, read_public_files


def package_source(root, output):
    """扫描一次字节快照并原子写入 ZIP；失败保留现有文件，不读取私人运行目录。"""
    files = read_public_files(root)
    issues = check_files(files)
    if issues:
        for issue in issues:
            print(f"{issue.path}:{issue.line}: {issue.rule}", file=sys.stderr)
        raise ValueError("public source check failed")
    for name in ("Cargo.toml", "Cargo.lock", "README.md", "LICENSE"):
        if name not in files:
            raise ValueError(f"missing required file: {name}")
    package = tomllib.loads(files["Cargo.toml"].decode("utf-8"))["package"]
    project_name, version = package["name"], package["version"]
    if not all(isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9_.-]+", value)
               for value in (project_name, version)):
        raise ValueError("invalid package name or version")
    prefix = f"{project_name}-{version}"
    if output.exists():
        raise ValueError("output already exists; choose a new output path")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".fmp-source-", dir=output.parent) as temporary:
        archive_path = Path(temporary) / "source.zip"
        with zipfile.ZipFile(archive_path, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
            for name, content in files.items():
                info = zipfile.ZipInfo(f"{prefix}/{name}", date_time=(1980, 1, 1, 0, 0, 0))
                info.create_system = 3
                info.external_attr = 0o100644 << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                archive.writestr(info, content, compresslevel=9)
        with zipfile.ZipFile(archive_path) as archive:
            if archive.testzip() is not None:
                raise ValueError("source archive integrity check failed")
        archive_path.replace(output)
    return len(files), hashlib.sha256(output.read_bytes()).hexdigest()


def main():
    """解析项目与显式输出路径，报告发布文件数和归档校验值。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        output = args.output.expanduser().resolve()
        count, digest = package_source(args.root.resolve(), output)
    except (OSError, ValueError, KeyError, TypeError):
        print("Source packaging failed; check source, manifest, output and permissions.", file=sys.stderr)
        return 1
    print(f"ZIP: {output}")
    print(f"Source files: {count}")
    print(f"SHA256: {digest}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
