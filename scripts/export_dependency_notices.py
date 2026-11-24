#!/usr/bin/env python3
"""从 Cargo 解析结果生成第三方依赖声明；需要 Python 3.11+，不复制作者信息。"""

import argparse
import json
from pathlib import Path
import subprocess
import sys
from urllib.parse import quote


def render_notices(metadata):
    """列出公开 crates 的确切版本与声明许可，保留组合许可的原始含义。"""
    packages = sorted(
        (package for package in metadata["packages"] if package.get("source")),
        key=lambda package: (package["name"], package["version"]),
    )
    for package in packages:
        if package["source"] != "registry+https://github.com/rust-lang/crates.io-index":
            raise ValueError("non-public dependency source; review before publishing")
        if not package.get("license"):
            raise ValueError("dependency has no declared license; review before publishing")
    lines = [
        "# 第三方依赖声明", "",
        "项目自身使用 [MIT 许可证](LICENSE)。本文件根据 `Cargo.lock` 对应的",
        "Cargo 解析结果生成；包含开发依赖与其他目标平台依赖，不表示每个依赖均链接进",
        "当前平台的二进制。源码包没有内嵌这些依赖的源代码。", "",
        "表中的许可表达式来自上游 crate 元数据，`OR` 表示可选许可，`AND` 表示同时适用；",
        "旧式斜杠表达保留上游写法。项目 MIT 许可不覆盖第三方依赖。",
        "依赖本身的版权、许可全文与 NOTICE 以对应版本的上游源码包为准。",
        "发布二进制时，应按实际链接的依赖保留相应版权、许可和归属声明。", "",
        "重新生成：", "",
        "```sh", "python3 -B scripts/export_dependency_notices.py", "```", "",
        "该命令执行 `cargo metadata --locked --format-version 1`，只接受公开 crates.io",
        "依赖。它可能需要下载尚未缓存的依赖元数据。", "",
        f"当前解析包含 **{len(packages)}** 个第三方 package 版本。", "",
        "| 依赖 | 版本 | 上游声明许可 |",
        "| --- | --- | --- |",
    ]
    for package in packages:
        name, version = package["name"], package["version"]
        license_name = package["license"].replace("|", r"\|")
        url = f"https://crates.io/crates/{quote(name, safe='')}/{quote(version, safe='')}"
        lines.append(f"| [{name}]({url}) | {version} | {license_name} |")
    return "\n".join(lines) + "\n"


def main():
    """读取公开的解析结果并生成声明，不输出本机 Cargo 路径与账号环境。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--metadata", type=Path, help="已有 cargo metadata JSON，可用于离线生成")
    parser.add_argument("--output", type=Path, default=Path("THIRD_PARTY_NOTICES.md"))
    args = parser.parse_args()
    try:
        if args.metadata:
            metadata = json.loads(args.metadata.read_text(encoding="utf-8"))
        else:
            result = subprocess.run(
                ["cargo", "metadata", "--locked", "--format-version", "1"],
                check=True, capture_output=True, text=True)
            metadata = json.loads(result.stdout)
        args.output.write_text(render_notices(metadata), encoding="utf-8")
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
        print("Dependency notice export failed; review Cargo metadata and licenses.", file=sys.stderr)
        return 1
    print("Third-party dependency notices generated.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
