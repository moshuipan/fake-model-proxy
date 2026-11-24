"""把已构建的 macOS 可执行文件与启动说明打成便携 ZIP。"""

import argparse
import hashlib
from pathlib import Path
import struct
import tomllib
import zipfile

MACHO_64_MAGIC = 0xFEEDFACF
MH_EXECUTE = 0x2
ARCHITECTURES = {
    0x0100000C: ("arm64", "Apple Silicon"),
    0x01000007: ("x64", "Intel"),
}


def validate_executable(path):
    """检查 Mach-O 架构与文件类型，并拒绝携带本机用户目录的可执行文件。"""
    content = path.read_bytes()
    if len(content) < 32 or struct.unpack_from("<I", content, 0)[0] != MACHO_64_MAGIC:
        raise ValueError("输入不是 64 位 Mach-O 可执行文件")
    machine, platform = ARCHITECTURES.get(
        struct.unpack_from("<i", content, 4)[0], (None, None))
    if machine is None:
        raise ValueError("不支持的 Mach-O CPU 架构")
    if struct.unpack_from("<I", content, 12)[0] != MH_EXECUTE:
        raise ValueError("输入不是可执行文件")
    for encoding in ("utf-8", "utf-16-le"):
        if str(Path.home()).encode(encoding) in content:
            raise ValueError("可执行文件包含本机用户目录；请使用路径映射重新编译")
    return content, machine, platform


def add_file(archive, name, data, mode=0o644):
    """写入文件并保留 Unix 权限位，保证解压后仍可直接运行。"""
    info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
    info.create_system = 3
    info.external_attr = (0o100000 | mode) << 16
    info.compress_type = zipfile.ZIP_DEFLATED
    archive.writestr(info, data, compresslevel=9)


def main():
    """读取版本与架构，生成 ZIP 并验证包的完整性。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("executable", type=Path)
    args = parser.parse_args()
    source = Path(__file__).resolve().parent
    project = source.parents[1]
    version = tomllib.loads((project / "Cargo.toml").read_text())["package"]["version"]
    executable, machine, platform = validate_executable(args.executable)
    name = f"fakemodelproxy-{version}-macos-{machine}"
    output = project.parent / f"{name}.zip"
    digest = hashlib.sha256(executable).hexdigest()
    info = (
        f"Version: {version}\n"
        f"Architecture: {machine} ({platform})\n"
        f"Executable SHA256: {digest}\n"
        "Code signing: none\n"
        "macOS runtime test: not performed\n"
    )
    instructions = (source / "使用说明.txt").read_text(
        encoding="utf-8").replace("{ARCH}", platform)
    with zipfile.ZipFile(output, "w") as archive:
        add_file(archive, f"{name}/fakemodelproxy", executable, 0o755)
        add_file(archive, f"{name}/start.command", (source / "start.command").read_bytes(), 0o755)
        add_file(archive, f"{name}/使用说明.txt", instructions.encode("utf-8"))
        add_file(archive, f"{name}/build-info.txt", info.encode("utf-8"))
        for filename in ("LICENSE", "THIRD_PARTY_NOTICES.md"):
            add_file(archive, f"{name}/{filename}", (project / filename).read_bytes())
    with zipfile.ZipFile(output) as archive:
        assert archive.testzip() is None
        assert len(archive.infolist()) == 6
    print(f"ZIP: {output}")
    print(f"Size: {output.stat().st_size} bytes")
    print(f"ZIP SHA256: {hashlib.sha256(output.read_bytes()).hexdigest()}")


if __name__ == "__main__":
    main()
