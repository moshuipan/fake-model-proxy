"""隔离采集器回归：只使用临时目录与固定系统信息，不读取真实客户端。"""

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("collector", Path(__file__).with_name("collect-trae-device.py"))
collector = importlib.util.module_from_spec(spec)
spec.loader.exec_module(collector)


class CollectorTests(unittest.TestCase):
    """验证注册证据、候选来源、白名单和平台数据，不进行联网调用。"""

    def setUp(self):
        """为每个用例创建独立客户端目录。"""
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.system = {"device_type": "mac", "device_brand": "Mac15,7", "os_version": "macOS 14.7.8"}

    def client(self, name, device="1234567890123456"):
        """创建带凭证哨兵的官方目录，确保输出不会夹带登录字段。"""
        profile = self.root / name
        product = self.root / (name + "-app") / "product.json"
        product.parent.mkdir()
        product.write_text(json.dumps({"appVersion": "3.4.24", "version": "1.107.1", "tronBuildVersion": "2.3.1", "quality": "stable"}))
        storage = profile / "User/globalStorage/storage.json"
        storage.parent.mkdir(parents=True)
        storage.write_text(json.dumps({"telemetry.machineId": "machine_" + name,
                                       "auth": {"Token": "SECRET_SENTINEL"}, "other": "PRIVATE_SENTINEL"}))
        log = profile / "logs/20261124T120000/main.log"
        log.parent.mkdir(parents=True)
        log.write_text("[ICDRS] (init) initialization done, did: " + device + ", ldid: aha-local, rdid: " + device)
        return profile, product

    def test_multiple_profiles_and_exact_string_id(self):
        """多配置不混合，重复路径不重复，长数字 ID 保持字符串。"""
        first = self.client("first")
        second = self.client("second", "99999999999999999999")
        report = collector.collect([first, second, first], self.system)
        self.assertEqual(len(report["candidates"]), 2)
        self.assertEqual(report["candidates"][1]["value"]["device_id"], "99999999999999999999")
        self.assertEqual(report["candidates"][0]["value"]["machine_id"], "machine_first")
        self.assertEqual(report["candidates"][0]["value"]["app_version"], "3.4.24")
        self.assertEqual(report["candidates"][0]["value"]["ide_version"], "1.107.1")
        self.assertNotIn("SECRET_SENTINEL", json.dumps(report))
        self.assertNotIn("PRIVATE_SENTINEL", json.dumps(report))

    def test_registered_evidence_excludes_fallbacks_and_urls(self):
        """日志中的 URL、machineId、备用 ID 和零值不能冒充已注册设备。"""
        profile, product = self.client("missing", "0")
        log = profile / "logs/20261124T120000/main.log"
        log.write_text(log.read_text() + "\nhttps://example.test?did=12345\n[ICDRS] ldid: aha-1234, rdid: aha-1234")
        report = collector.collect([(profile, product)], self.system)
        self.assertEqual(report["candidates"], [])
        self.assertIn("device_id", report["warnings"][0])

    def test_latest_registration_and_corrupt_storage(self):
        """选最新启动记录，坏 JSON 不会把文件原文放入错误或结果。"""
        profile, product = self.client("newest")
        log = profile / "logs/20261125T120000/main.log"
        log.parent.mkdir(parents=True)
        log.write_text("[ICDRS] (init) resolve rdid: 987654321")
        (profile / "User/globalStorage/storage.json").write_text('{"Token":"SECRET_SENTINEL", broken')
        report = collector.collect([(profile, product)], self.system)
        self.assertEqual(report["candidates"][0]["value"]["device_id"], "987654321")
        self.assertEqual(report["candidates"][0]["value"]["machine_id"], "")
        self.assertNotIn("SECRET_SENTINEL", json.dumps(report))

    def test_running_custom_profile_is_detected(self):
        """正在运行的自定义目录优先发现，主进程之外的 Helper 不作为客户端。"""
        profile, product = self.client("custom")
        app = self.root / "Trae CN.app"
        target = app / "Contents/Resources/app/product.json"
        target.parent.mkdir(parents=True)
        target.write_text(product.read_text())
        output = str(app / "Contents/MacOS/Electron") + ' --user-data-dir="' + str(profile) + '"'
        with patch.object(collector.platform, "system", return_value="Darwin"), patch.object(collector, "command", return_value=output):
            self.assertEqual(collector.running_pairs(), [(profile, target)])

    def test_platform_values_follow_official_os_names(self):
        """macOS、Windows 和 Linux 的型号及 OS 格式均来自系统读取。"""
        with patch.object(collector.platform, "system", return_value="Darwin"), patch.object(collector, "command", side_effect=["14.7.8", "Mac15,7"]):
            self.assertEqual(collector.system_device(), self.system)
        with patch.object(collector.platform, "system", return_value="Windows"), patch.object(collector, "windows_system", return_value={"model": "Surface Laptop", "caption": "Microsoft Windows 11 Pro"}):
            self.assertEqual(collector.system_device(), {"device_type": "windows", "device_brand": "Surface Laptop", "os_version": "Windows 11 Pro"})
        with patch.object(collector.platform, "system", return_value="Linux"), patch.object(Path, "read_text", side_effect=['PRETTY_NAME="Ubuntu 24.04.3 LTS"\nVERSION_ID="24.04"', "ThinkPad\n"]):
            self.assertEqual(collector.system_device(), {"device_type": "linux", "device_brand": "ThinkPad", "os_version": "Ubuntu 24.04.3 LTS"})


if __name__ == "__main__":
    unittest.main()
