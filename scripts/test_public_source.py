"""验证发布边界、敏感信息诊断和源码归档，不读取真实账号或数据目录。"""

from contextlib import redirect_stderr
import hashlib
import io
from pathlib import Path
import tempfile
import unittest
import zipfile

from check_public_source import (
    check_files, check_package_list, is_public_path, read_public_files,
)
from package_source import package_source


class PublicSourceTests(unittest.TestCase):
    """对公开源码和运行数据的边界进行行为回归。"""

    def setUp(self):
        """为每个测试生成可独立发布的最小项目和私有数据哨兵。"""
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.write("Cargo.toml", '[package]\nname = "fmp-test"\nversion = "0.1.0"\n')
        self.write("Cargo.lock", "version = 4\n")
        self.write("README.md", "[Guide](docs/guide.md)\n")
        self.write("LICENSE", "Synthetic test license\n")
        self.write("docs/guide.md", "[Home](../README.md)\n")
        self.write("src/lib.rs", "//! Synthetic test source.\n")
        self.write(".github/workflows/ci.yml", "name: CI\n")
        self.write(".fakemodelproxy/requests.jsonl", "PRIVATE_SENTINEL")
        self.write(".minimax-oauth-preview/token.json", "SECRET_SENTINEL")
        self.write("target/debug/fmp", "BUILD_SENTINEL")
        self.write("scripts/device-report.json", "DEVICE_SENTINEL")

    def write(self, name, text):
        """在临时项目中写入测试文件。"""
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def test_whitelist_excludes_runtime_and_artifacts(self):
        """发布包含源码与 GitHub 配置，不包含隐藏状态、报告或构建产物。"""
        files = read_public_files(self.root)
        self.assertIn(".github/workflows/ci.yml", files)
        self.assertEqual(check_files(files), [])
        for name in (".fakemodelproxy/requests.jsonl", "target/debug/fmp",
                     "scripts/device-report.json", "../secret.rs", "/src/lib.rs"):
            self.assertFalse(is_public_path(name))
            self.assertNotIn(name, files)

    def test_console_images_are_public_binary_assets(self):
        """控制台截图只允许放在 docs/images，二进制内容不做文本解码。"""
        image = self.root / "docs/images/console-overview.png"
        image.parent.mkdir(parents=True, exist_ok=True)
        image.write_bytes(b"\x89PNG\r\n\x1a\n" + b"synthetic")
        files = read_public_files(self.root)
        self.assertIn("docs/images/console-overview.png", files)
        self.assertEqual(check_files(files), [])
        for name in ("docs/console-overview.png", "docs/images/nested/console.png"):
            self.assertFalse(is_public_path(name))

    def test_sensitive_issues_do_not_reveal_values(self):
        """真实模式触发定位诊断，但不将合成凭证原文带入诊断。"""
        token = "ghp_" + "A" * 40
        domain = "private." + "bytedance" + ".net"
        home = "/" + "Users/" + "private-user/project/"
        files = {"README.md": "\n".join((token, domain, home)).encode()}
        issues = check_files(files)
        self.assertEqual({issue.rule for issue in issues},
                         {"github-token", "enterprise-domain", "private-home-path"})
        self.assertEqual({issue.line for issue in issues}, {1, 2, 3})
        self.assertNotIn(token, repr(issues))
        self.assertNotIn(domain, repr(issues))

    def test_links_are_checked_against_published_files(self):
        """链接到私人文件或缺失文档不能通过，即使本机文件存在。"""
        self.write("README.md", "[Missing](missing.md)\n[Private](.fakemodelproxy/requests.jsonl)\n")
        issues = check_files(read_public_files(self.root))
        self.assertEqual(len(issues), 2)
        self.assertTrue(all(issue.rule == "missing-document-link" for issue in issues))

    def test_symlinks_cannot_import_external_files(self):
        """公开文件的符号链接不能带入白名单外数据。"""
        (self.root / "src/lib.rs").unlink()
        (self.root / "src/lib.rs").symlink_to(self.root / ".fakemodelproxy/requests.jsonl")
        with self.assertRaisesRegex(ValueError, "symlink"):
            read_public_files(self.root)

    def test_cargo_list_rejects_runtime_data(self):
        """Cargo 生成的清单元数据可以通过，运行数据库不可以通过。"""
        files = read_public_files(self.root)
        self.assertEqual(check_package_list(["Cargo.toml.orig", "Cargo.lock"], files), [])
        issues = check_package_list([".fakemodelproxy/fakemodelproxy.db"], files)
        self.assertEqual(issues[0].rule, "outside-public-allowlist")

    def test_archive_is_reproducible_and_contains_only_source(self):
        """相同字节生成相同 ZIP，归档不含私人哨兵，并拒绝覆盖已有归档。"""
        first, second = self.root / "first.zip", self.root / "second.zip"
        count, digest = package_source(self.root, first)
        self.assertEqual(package_source(self.root, second), (count, digest))
        self.assertEqual(hashlib.sha256(first.read_bytes()).hexdigest(), digest)
        with zipfile.ZipFile(first) as archive:
            self.assertEqual(len(archive.namelist()), count)
            self.assertEqual(archive.testzip(), None)
            for name in archive.namelist():
                self.assertTrue(name.startswith("fmp-test-0.1.0/"))
                self.assertNotIn(b"PRIVATE_SENTINEL", archive.read(name))
                self.assertNotIn(b"SECRET_SENTINEL", archive.read(name))
        with self.assertRaises(ValueError):
            package_source(self.root, first)

    def test_sensitive_source_prevents_creating_archive(self):
        """发现敏感源码时不生成归档，也不在失败日志中打印凭证。"""
        token = "sk-proj-" + "A" * 40
        self.write("src/lib.rs", token)
        output = self.root / "source.zip"
        diagnostic = io.StringIO()
        with redirect_stderr(diagnostic), self.assertRaises(ValueError):
            package_source(self.root, output)
        self.assertFalse(output.exists())
        self.assertIn("provider-token", diagnostic.getvalue())
        self.assertNotIn(token, diagnostic.getvalue())


if __name__ == "__main__":
    unittest.main()
