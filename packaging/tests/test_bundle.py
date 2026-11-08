"""Regression tests at the package boundary; no device access or root required."""
import importlib.machinery
import importlib.util
import json
import pathlib
import subprocess
import tempfile
import unittest
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
loader = importlib.machinery.SourceFileLoader('bundle', str(ROOT / 'packaging/verify-bundle'))
spec = importlib.util.spec_from_loader(loader.name, loader)
bundle = importlib.util.module_from_spec(spec)
loader.exec_module(bundle)


def elf(version=5, machine=62):
    header = bytearray(64)
    header[:6] = b'\x7fELF\x02\x01'
    header[18:20] = machine.to_bytes(2, 'little')
    return bytes(header) + f'DROIDLOOM_INPUT_ABI={version:05d};'.encode()


class BundleTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name)
        self.put(bundle.COMPOSER, elf() + bundle.TASK_ABI + bundle.DIRECT_INPUT_ABI + bundle.NATIVE_INPUT_ABI)
        for name in ['droidloomd', 'droidloomctl', 'droidloom-supervisor', 'droidloom-wayland', 'droidloom-applications', 'droidloom-audio', 'droidloom-gpu-server', 'droidloom-devices']:
            self.put(pathlib.Path('usr/bin') / name, elf() + bundle.FRAMEWORK_ABI)
        self.put(pathlib.Path('usr/bin/droidloom-wayland'), elf() + bundle.DIRECT_INPUT_ABI + bundle.CHANNELS_ABI + bundle.NOTIFICATIONS_ABI)
        self.jar(bundle.RUNTIME / 'systemui/SystemUI.apk', 5)
        self.jar(bundle.RUNTIME / 'ime/DroidloomIME.apk', 5)
        self.put(bundle.POLICY_NATIVE, elf() + bundle.POLICY_ABI + bundle.TASK_ABI + bundle.NATIVE_INPUT_ABI + bundle.CHANNELS_ABI + bundle.FRAMEWORK_ABI)
        path = self.root / bundle.POLICY_JAR
        path.parent.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(path, 'w') as z:
            z.writestr('classes.dex', bundle.POLICY_ABI + bundle.TASK_ABI + bundle.NATIVE_INPUT_ABI + bundle.CHANNELS_ABI + bundle.FRAMEWORK_ABI + b'Lcom/android/server/wm/DroidloomApplicationCatalog;')

    def put(self, path, data):
        path = self.root / path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)

    def test_direct_framework_peers_and_notification_deltas_must_match(self):
        for relative, marker in [(bundle.POLICY_JAR, bundle.FRAMEWORK_ABI),
                                 (bundle.POLICY_NATIVE, bundle.FRAMEWORK_ABI),
                                 (pathlib.Path('usr/bin/droidloomd'), bundle.FRAMEWORK_ABI),
                                 (pathlib.Path('usr/bin/droidloom-applications'), bundle.FRAMEWORK_ABI),
                                 (pathlib.Path('usr/bin/droidloom-wayland'), bundle.NOTIFICATIONS_ABI),
                                 (bundle.RUNTIME / 'systemui/SystemUI.apk', bundle.NOTIFICATIONS_ABI)]:
            with self.subTest(artifact=relative):
                path = self.root / relative
                original = path.read_bytes()
                if path.suffix in ['.jar', '.apk']:
                    with zipfile.ZipFile(path) as archive:
                        data = archive.read('classes.dex')
                    with zipfile.ZipFile(path, 'w') as archive:
                        archive.writestr('classes.dex', data.replace(marker, b'OLD_ABI'))
                else:
                    path.write_bytes(original.replace(marker, b'OLD_ABI'))
                with self.assertRaises(ValueError): bundle.seal(self.root)
                path.write_bytes(original)

    def jar(self, path, version):
        path = self.root / path
        path.parent.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(path, 'w') as z:
            z.writestr('classes.dex', f'DROIDLOOM_INPUT_ABI={version};'.encode() + bundle.TASK_ABI + bundle.NATIVE_INPUT_ABI + bundle.CHANNELS_ABI + bundle.NOTIFICATIONS_ABI)

    def test_native_receiver_cannot_be_mixed_with_old_support_or_framework(self):
        for relative in [bundle.POLICY_NATIVE, bundle.POLICY_JAR, bundle.COMPOSER]:
            with self.subTest(artifact=relative):
                path = self.root / relative
                original = path.read_bytes()
                if path.suffix == '.jar':
                    with zipfile.ZipFile(path) as z:
                        dex = z.read('classes.dex')
                    with zipfile.ZipFile(path, 'w') as z:
                        z.writestr('classes.dex', dex.replace(bundle.NATIVE_INPUT_ABI, b''))
                else:
                    path.write_bytes(original.replace(bundle.NATIVE_INPUT_ABI, b''))
                with self.assertRaisesRegex(ValueError, 'native input ABI'):
                    bundle.seal(self.root)
                path.write_bytes(original)

    def test_matching_bundle(self):
        bundle.seal(self.root)
        self.assertEqual(bundle.verify(self.root)['input_protocol'], 5)

    def test_direct_input_requires_matching_composer_and_presenter(self):
        for relative in [bundle.COMPOSER, pathlib.Path('usr/bin/droidloom-wayland')]:
            path = self.root / relative
            original = path.read_bytes()
            path.write_bytes(original.replace(bundle.DIRECT_INPUT_ABI, b''))
            with self.assertRaisesRegex(ValueError, 'direct input ABI'):
                bundle.seal(self.root)
            path.write_bytes(original)

    def test_stock_or_mismatched_policy_native_is_rejected(self):
        for data in [elf(), elf() + b'DROIDLOOM_WINDOW_POLICY_ABI=2;',
                     elf(machine=183) + bundle.POLICY_ABI]:
            with self.subTest(data=data):
                self.put(bundle.POLICY_NATIVE, data)
                with self.assertRaisesRegex(ValueError, 'Rust window policy'):
                    bundle.seal(self.root)

    def test_old_task_launcher_cannot_be_paired_with_new_framework(self):
        self.put(bundle.RUNTIME / 'bin/droidloom-task-launcher', elf())
        with self.assertRaisesRegex(ValueError, 'retired support artifact'):
            bundle.seal(self.root)

    def test_previous_task_backend_cannot_be_mixed_with_direct_window_controls(self):
        for relative in [bundle.COMPOSER, bundle.POLICY_NATIVE, bundle.POLICY_JAR]:
            with self.subTest(artifact=relative):
                path = self.root / relative
                original = path.read_bytes()
                if path.suffix == '.jar':
                    with zipfile.ZipFile(path) as z:
                        dex = z.read('classes.dex')
                    with zipfile.ZipFile(path, 'w') as z:
                        z.writestr('classes.dex', dex.replace(bundle.TASK_ABI, b'DROIDLOOM_TASK_BACKEND_ABI=1;'))
                else:
                    path.write_bytes(original.replace(bundle.TASK_ABI, b'DROIDLOOM_TASK_BACKEND_ABI=1;'))
                with self.assertRaisesRegex(ValueError, 'task backend ABI'):
                    bundle.seal(self.root)
                path.write_bytes(original)

    def test_exact_reported_regression_old_sender_new_bridge(self):
        self.put(bundle.COMPOSER, elf(4) + bundle.TASK_ABI + bundle.DIRECT_INPUT_ABI + bundle.NATIVE_INPUT_ABI)
        with self.assertRaisesRegex(ValueError, 'Composer v4.*v5'):
            bundle.seal(self.root)
        self.assertFalse((self.root / bundle.MANIFEST).exists())

    def test_legacy_binary_cannot_be_stamped_from_current_sources(self):
        self.put(bundle.COMPOSER, elf().split(b'DROIDLOOM_')[0])
        with self.assertRaisesRegex(ValueError, 'missing.*metadata'):
            bundle.seal(self.root)

    def test_retired_support_process_is_rejected(self):
        self.jar(bundle.RUNTIME / 'ime/droidloom-input-bridge.jar', 4)
        with self.assertRaisesRegex(ValueError, 'retired support artifact'):
            bundle.seal(self.root)

    def test_modification_missing_extra_and_mode_are_rejected(self):
        for mutation in ['replace', 'remove', 'extra', 'mode']:
            with self.subTest(mutation=mutation):
                self.put('fixture', b'original')
                bundle.seal(self.root)
                path = self.root / 'fixture'
                if mutation == 'replace': path.write_bytes(b'changed')
                elif mutation == 'remove': path.unlink()
                elif mutation == 'extra': self.put('unexpected', b'extra')
                else: path.chmod(0o700)
                with self.assertRaisesRegex(ValueError, 'changed after sealing'):
                    bundle.verify(self.root)
                if (self.root / 'unexpected').exists(): (self.root / 'unexpected').unlink()

    def test_forged_contract_disagrees_with_artifacts(self):
        bundle.seal(self.root)
        path = self.root / bundle.MANIFEST
        manifest = json.loads(path.read_text())
        manifest['compatibility']['input_protocol'] = 4
        path.write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, 'disagrees'):
            bundle.verify(self.root)

    def test_host_and_android_architectures_match(self):
        self.put('usr/bin/droidloomctl', elf(machine=183))
        with self.assertRaisesRegex(ValueError, 'architectures'):
            bundle.seal(self.root)

    def test_external_symlink_is_rejected(self):
        (self.root / 'escape').symlink_to('/etc/passwd')
        with self.assertRaisesRegex(ValueError, 'symlink'):
            bundle.seal(self.root)

    def test_cli_reports_failure(self):
        result = subprocess.run(['python3', str(ROOT / 'packaging/verify-bundle'), 'verify', str(self.root)], capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b'bundle rejected', result.stderr)


if __name__ == '__main__':
    unittest.main()
