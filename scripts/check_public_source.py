#!/usr/bin/env python3
"""检查公开源码白名单、敏感标识与文档链接；诊断不回显文件正文。"""

import argparse
from dataclasses import dataclass
import os
from pathlib import Path, PurePosixPath
import posixpath
import re
import sys
from urllib.parse import unquote, urlsplit

ROOT_FILES = frozenset({
    "Cargo.toml", "Cargo.lock", "README.md", "LICENSE", "CONTRIBUTING.md",
    "SECURITY.md", "CODE_OF_CONDUCT.md", "CHANGELOG.md", "THIRD_PARTY_NOTICES.md",
    "AGENTS.md", "rust-toolchain.toml", ".gitignore", ".gitattributes",
    ".editorconfig", ".env.example",
})
SOURCE_DIRS = frozenset({
    "src", "web", "presets", "tests", "scripts", "examples", "packaging", "docs",
    ".github",
})
SUFFIXES = {
    "src": {".rs", ".txt"},
    "web": {".html", ".css", ".js", ".cjs"},
    "presets": {".json"},
    "tests": {".rs"},
    "scripts": {".py"},
    "examples": {".rs"},
    "packaging": {".py", ".cmd", ".command", ".txt", ".md"},
    "docs": {".md", ".png"},
    ".github": {".yml", ".yaml", ".md"},
}
PNG_MAGIC = b"\x89PNG\r\n\x1a\n"
SKIP_DIRS = frozenset({"__pycache__", "node_modules", "target", ".git"})
CARGO_GENERATED = frozenset({"Cargo.toml.orig", ".cargo_vcs_info.json"})
SENSITIVE_PATTERNS = (
    ("enterprise-domain", re.compile(
        r"\b(?:[a-z0-9-]+\.)*(?:bytedance\.net|byted\.org|byteintl\.net|"
        r"bytedance\.org|larkoffice\.net|feishu\.net)\b", re.IGNORECASE)),
    ("enterprise-email", re.compile(
        r"\b[\w.+-]+@(?:bytedance\.com|bytedance\.net|byteintl\.net)\b",
        re.IGNORECASE)),
    ("private-home-path", re.compile(r"/(?:Users|home)/[A-Za-z0-9._-]+(?:/|\b)")),
    ("private-windows-path", re.compile(
        r"\b[A-Za-z]:[\\/]+Users[\\/]+[A-Za-z0-9._-]+(?:[\\/]+|\b)", re.IGNORECASE)),
    ("private-key", re.compile(r"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----")),
    ("github-token", re.compile(
        r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{50,})\b")),
    ("provider-token", re.compile(
        r"\bsk-(?:proj-|ant-api\d{2}-)?[A-Za-z0-9_-]{24,}\b")),
    ("cloud-access-key", re.compile(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b")),
    ("jwt", re.compile(
        r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{20,}\b")),
)
MARKDOWN_LINK = re.compile(r"!?\[[^\]\n]*\]\(\s*(<[^>\n]+>|[^\s)]+)")


@dataclass(frozen=True)
class Issue:
    """保存可公开的定位信息，不保存敏感原文。"""

    path: str
    line: int
    rule: str


def is_public_path(name):
    """判断相对路径是否属于允许发布的文本源码，拒绝路径穿越与隐藏状态。"""
    path = PurePosixPath(name)
    if not name or "\\" in name or path.is_absolute() or ".." in path.parts:
        return False
    if name in ROOT_FILES:
        return True
    if len(path.parts) < 2 or path.parts[0] not in SOURCE_DIRS:
        return False
    if any(part.startswith(".") or part in SKIP_DIRS for part in path.parts[1:]):
        return False
    if path.parts[0] in {"web", "presets", "tests", "scripts", "examples"} and len(path.parts) != 2:
        return False
    if path.parts[0] == "docs" and path.suffix == ".json":
        return len(path.parts) == 3 and path.parts[1] == "generated"
    if path.parts[0] == "docs" and path.suffix == ".png":
        return len(path.parts) == 3 and path.parts[1] == "images"
    return path.suffix in SUFFIXES[path.parts[0]]


def read_public_files(root):
    """仅遍历公开目录并读取白名单文件；拒绝符号链接，避免带入目录外数据。"""
    files = {}
    for name in sorted(ROOT_FILES):
        path = root / name
        if path.is_symlink():
            raise ValueError(f"{name}: symlink")
        if path.is_file():
            files[name] = path.read_bytes()
    for directory in sorted(SOURCE_DIRS):
        base = root / directory
        if base.is_symlink():
            raise ValueError(f"{directory}: symlink")
        if not base.is_dir():
            continue
        for current, directories, names in os.walk(base, followlinks=False):
            directories[:] = sorted(
                name for name in directories if name not in SKIP_DIRS and not name.startswith("."))
            for name in directories:
                path = Path(current) / name
                if path.is_symlink():
                    raise ValueError(f"{path.relative_to(root).as_posix()}: symlink")
            for name in sorted(names):
                path = Path(current) / name
                relative = path.relative_to(root).as_posix()
                if is_public_path(relative):
                    if path.is_symlink():
                        raise ValueError(f"{relative}: symlink")
                    files[relative] = path.read_bytes()
    return dict(sorted(files.items()))


def check_markdown_links(name, text, files):
    """校验相对文档链接在发布源码中可解析，拒绝本机 file URL 和绝对路径。"""
    issues = []
    for match in MARKDOWN_LINK.finditer(text):
        target = match.group(1).strip("<>")
        line = text.count("\n", 0, match.start()) + 1
        try:
            url = urlsplit(target)
        except ValueError:
            issues.append(Issue(name, line, "invalid-document-link"))
            continue
        if url.scheme == "file" or (not url.scheme and url.path.startswith("/")):
            issues.append(Issue(name, line, "private-document-link"))
        elif not url.scheme and not url.netloc and url.path:
            resolved = posixpath.normpath(posixpath.join(
                posixpath.dirname(name), unquote(url.path)))
            if resolved not in files and not any(
                    entry.startswith(resolved.rstrip("/") + "/") for entry in files):
                issues.append(Issue(name, line, "missing-document-link"))
    return issues


def check_files(files):
    """扫描实际发布的字节快照，返回规则名与位置；不读取运行状态或输出正文。"""
    issues = []
    for name, content in files.items():
        if not is_public_path(name):
            issues.append(Issue(name, 0, "outside-public-allowlist"))
            continue
        if name.endswith(".png"):
            if not content.startswith(PNG_MAGIC):
                issues.append(Issue(name, 0, "invalid-image"))
            continue
        try:
            text = content.decode("utf-8-sig")
        except UnicodeError:
            issues.append(Issue(name, 0, "non-text-source"))
            continue
        if "\0" in text:
            issues.append(Issue(name, 0, "non-text-source"))
        for rule, pattern in SENSITIVE_PATTERNS:
            for match in pattern.finditer(text):
                issues.append(Issue(name, text.count("\n", 0, match.start()) + 1, rule))
        if name.endswith(".md"):
            issues.extend(check_markdown_links(name, text, files))
    return sorted(set(issues), key=lambda issue: (issue.path, issue.line, issue.rule))


def check_package_list(names, files):
    """检查 Cargo 的实际发布列表，允许其生成的元数据，拒绝秘密与缺失源码。"""
    issues = []
    for name in names:
        if name in CARGO_GENERATED:
            continue
        if not is_public_path(name):
            issues.append(Issue(name, 0, "outside-public-allowlist"))
        elif name not in files:
            issues.append(Issue(name, 0, "missing-package-source"))
    return issues


def main():
    """检查当前项目及可选 Cargo 列表，失败时只输出路径、行号和规则名。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--package-list", type=Path, help="cargo package --list 的 stdout 文件")
    args = parser.parse_args()
    try:
        files = read_public_files(args.root.resolve())
        if not files:
            raise ValueError("empty public source")
        issues = check_files(files)
        if args.package_list:
            names = args.package_list.read_text(encoding="utf-8").splitlines()
            if not names:
                raise ValueError("empty Cargo package list")
            issues.extend(check_package_list(names, files))
        for issue in issues:
            print(f"{issue.path}:{issue.line}: {issue.rule}", file=sys.stderr)
        if issues:
            return 1
        print(f"Public source check passed: {len(files)} files")
        return 0
    except (OSError, ValueError) as error:
        # OS 错误可能带本机完整路径，不将异常原文写入公开日志。
        reason = str(error) if isinstance(error, ValueError) else "unable to read source"
        print(f"Public source check failed: {reason}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
