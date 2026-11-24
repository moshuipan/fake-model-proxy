"""把已构建的 Windows x64 exe 与启动说明打成便携 ZIP。"""

import argparse
import hashlib
from pathlib import Path
import struct
import tomllib
import zipfile


def validate_executable(path):
    """检查 PE 架构，并拒绝携带本机用户目录的可执行文件。"""
    content = path.read_bytes()
    if len(content) < 64 or content[:2] != b"MZ":
        raise ValueError("输入不是 Windows PE 可执行文件")
    header = struct.unpack_from("<I", content, 0x3C)[0]
    if header + 26 > len(content) or content[header:header + 4] != b"PE\0\0":
        raise ValueError("PE 文件头不完整")
    if struct.unpack_from("<H", content, header + 4)[0] != 0x8664:
        raise ValueError("输入不是 Windows x64")
    if struct.unpack_from("<H", content, header + 24)[0] != 0x20B:
        raise ValueError("输入不是 PE32+")
    for encoding in ("utf-8", "utf-16-le"):
        if str(Path.home()).encode(encoding) in content:
            raise ValueError("exe 包含本机用户目录；请使用路径映射重新编译")
    return content


def main():
    """读取版本、转换启动文件为 CRLF，并验证生成的 ZIP 完整性。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("executable", type=Path)
    args = parser.parse_args()
    source = Path(__file__).resolve().parent
    project = source.parents[1]
    version = tomllib.loads((project / "Cargo.toml").read_text())["package"]["version"]
    name = f"fakemodelproxy-{version}-windows-x64"
    output = project.parent / f"{name}.zip"
    executable = validate_executable(args.executable)
    digest = hashlib.sha256(executable).hexdigest()
    info = (
        f"Version: {version}\r\n"
        "Target: x86_64-pc-windows-msvc\r\n"
        "C runtime: static\r\n"
        f"Executable SHA256: {digest}\r\n"
        "Windows runtime test: not performed\r\n"
    )
    with zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        archive.writestr(f"{name}/fakemodelproxy.exe", executable)
        for filename, encoding in [("start.cmd", "utf-8"), ("使用说明.txt", "utf-8-sig")]:
            text = (source / filename).read_text(encoding="utf-8").replace("\n", "\r\n")
            archive.writestr(f"{name}/{filename}", text.encode(encoding))
        archive.writestr(f"{name}/build-info.txt", info.encode("utf-8"))
        for filename in ("LICENSE", "THIRD_PARTY_NOTICES.md"):
            archive.writestr(f"{name}/{filename}", (project / filename).read_bytes())
    with zipfile.ZipFile(output) as archive:
        assert archive.testzip() is None
        assert len(archive.infolist()) == 6
    print(f"ZIP: {output}")
    print(f"Size: {output.stat().st_size} bytes")
    print(f"ZIP SHA256: {hashlib.sha256(output.read_bytes()).hexdigest()}")


if __name__ == "__main__":
    main()
