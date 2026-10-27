# Credential/archive regressions adapted from Ratatoskr (MIT, Oana Goge).
from contextlib import redirect_stderr, redirect_stdout
import io
import json
from pathlib import Path
import stat
import sys
import subprocess
import tempfile
import tarfile
import unittest
from unittest.mock import patch
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from macos_signing import (APP, accepted, cleanup_preserving_error, extract_updater,
                          notarize, pack_updater, remove_notary_container,
                          remove_keychain, run, security, validate_candidate_archive,
                          verify_publisher as verify)



class LocalPublisherBoundaries(unittest.TestCase):

    def test_credential_commands_do_not_expose_failure_output(self):
        failure = subprocess.CompletedProcess([], 1, stdout=b"private output", stderr=b"private output")
        with patch("macos_signing.subprocess.run", return_value=failure):
            with self.assertRaisesRegex(ValueError, "credential output suppressed") as error:
                run("gopass", "show", "entry", sensitive=True)
            self.assertNotIn("private output", str(error.exception))

    def test_keychain_diagnostics_redact_supplied_password(self):
        for password in ["secret-password", "-secret-password"]:
            failure = subprocess.CompletedProcess([], 1, stdout=b"", stderr=f"SecKeychainItemImport: {password} rejected".encode())
            with patch("macos_signing.subprocess.run", return_value=failure) as consumer:
                with self.assertRaises(ValueError) as error:
                    security("import", "encrypted.p12", "-P", password, redactions=(password,))
                self.assertNotIn(password, str(error.exception))
                self.assertNotIn(password, str(consumer.call_args.args))
                self.assertIn(password.encode(), consumer.call_args.kwargs["input"])

    @unittest.skipUnless(sys.platform == "darwin", "requires macOS codesign")
    def test_valid_ad_hoc_code_is_rejected_as_publisher_code(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "main.c"
            binary = Path(directory) / "probe"
            source.write_text("int main(void) { return 0; }\n")
            run("xcrun", "clang", source, "-o", binary)
            run("codesign", "--force", "--sign", "-", "--identifier", "ratatoskr", binary)
            run("codesign", "--verify", "--strict", binary)
            with self.assertRaisesRegex(ValueError, "codesign failed"):
                verify(binary, "ratatoskr", "ABCDE12345")

    def test_only_final_apple_acceptance_passes(self):
        accepted({"status": "Accepted"})
        for result in [{}, {"status": "Rejected"}, {"status": "In Progress"}]:
            with self.assertRaisesRegex(ValueError, "did not accept"):
                accepted(result)


    def test_candidate_rejects_path_traversal_and_external_symlinks(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "candidate.zip"
            for name in ["/outside", f"{APP}/../outside", "outside/file", "outside\\file"]:
                with zipfile.ZipFile(path, "w") as archive:
                    archive.writestr(name, "untrusted candidate")
                with self.assertRaises(ValueError):
                    validate_candidate_archive(path)
            for target in ["/outside", "../../outside"]:
                with zipfile.ZipFile(path, "w") as archive:
                    link = zipfile.ZipInfo(f"{APP}/escape")
                    link.external_attr = (stat.S_IFLNK | 0o777) << 16
                    archive.writestr(link, target)
                with self.assertRaisesRegex(ValueError, "link or special"):
                    validate_candidate_archive(path)

    def test_candidate_rejects_links_special_files_and_appledouble(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "candidate.zip"
            for mode in [stat.S_IFLNK, stat.S_IFIFO, stat.S_IFCHR]:
                with zipfile.ZipFile(path, "w") as archive:
                    link = zipfile.ZipInfo(f"{APP}/link")
                    link.external_attr = (mode | 0o777) << 16
                    archive.writestr(link, "internal-or-external-target")
                with self.assertRaisesRegex(ValueError, "link or special"):
                    validate_candidate_archive(path)
            for name in [f"__MACOSX/{APP}/metadata", f"{APP}/Contents/._binary"]:
                with zipfile.ZipFile(path, "w") as archive:
                    archive.writestr(name, "metadata")
                with self.assertRaises(ValueError):
                    validate_candidate_archive(path)

    def test_updater_archive_roundtrip_has_only_app_files_without_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            app = root / APP
            (app / "Contents/MacOS").mkdir(parents=True)
            payload = app / "Contents/MacOS/ratatoskr"
            payload.write_bytes(b"public binary bytes")
            if sys.platform == "darwin":
                run("/usr/bin/xattr", "-w", "com.ratatoskr.archive-test", "metadata", app)
            archive = root / "updater.tar.gz"
            pack_updater(app, archive)
            with tarfile.open(archive) as file:
                names = file.getnames()
            self.assertEqual(names, [APP, f"{APP}/Contents", f"{APP}/Contents/MacOS", f"{APP}/Contents/MacOS/ratatoskr"])
            output = root / "extracted"
            extract_updater(archive, output)
            self.assertEqual((output / APP / "Contents/MacOS/ratatoskr").read_bytes(), payload.read_bytes())

    def test_updater_rejects_appledouble_paths_before_extraction(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in [f"._{APP}", f"{APP}/Contents/MacOS/._ratatoskr", f"{APP}/../escape"]:
                archive = root / "unsafe.tar.gz"
                with tarfile.open(archive, "w:gz") as file:
                    member = tarfile.TarInfo(name)
                    member.size = 1
                    file.addfile(member, io.BytesIO(b"x"))
                destination = root / "must-not-be-created"
                with self.assertRaisesRegex(ValueError, "Unsafe updater"):
                    extract_updater(archive, destination)
                self.assertFalse(destination.exists())

    def test_cleanup_failure_keeps_original_failure_visible(self):
        def fail_cleanup():
            raise OSError("cannot remove credential container")
        with self.assertRaisesRegex(ValueError, "build failed; credential cleanup failed"):
            try:
                raise ValueError("build failed")
            finally:
                cleanup_preserving_error(fail_cleanup)


    def test_notary_key_is_removed_before_detach_and_force_retry(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "owned-container"
            mount = root / "mounted"
            mount.mkdir(parents=True)
            key = mount / "credential.p8"
            key.write_bytes(b"disposable test input")
            calls = []
            def detach(*args):
                self.assertFalse(key.exists())
                calls.append(args)
                if len(calls) == 1:
                    raise ValueError("mount busy")
                return b""
            with patch("macos_signing.run", side_effect=detach), patch("macos_signing.os.path.ismount", side_effect=[True, False]):
                remove_notary_container(root, mount, key)
            self.assertEqual(len(calls), 2)
            self.assertIn("-force", calls[1])
            self.assertFalse(root.exists())

    def test_failed_keychain_deletion_locks_and_preserves_the_container(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            keychain = root / "owned.keychain-db"
            keychain.write_bytes(b"encrypted test fixture")
            with patch("macos_signing.run", side_effect=[ValueError("deletion failed"), b""]) as consumer:
                with self.assertRaisesRegex(ValueError, "deletion failed"):
                    remove_keychain(root, keychain)
            self.assertEqual(consumer.call_args.args[1], "lock-keychain")
            self.assertTrue(keychain.exists())


    def test_notary_parsing_preserves_receipts_and_rejection_logs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifact = root / "public.zip"
            artifact.write_bytes(b"public candidate")
            log = root / "notary.json"
            for status in ["Accepted", "Invalid", "In Progress"]:
                response = subprocess.CompletedProcess([], 0, stdout=json.dumps({"id": "submission", "status": status}).encode(), stderr=b"")
                replies = [response, subprocess.CompletedProcess([], 0, stdout=b'{"issues":[]}', stderr=b"")]
                with patch("macos_signing.notary_command", side_effect=replies) as command, redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
                    if status == "Accepted":
                        notarize(artifact, "entry", log)
                    else:
                        with self.assertRaisesRegex(ValueError, "did not accept"):
                            notarize(artifact, "entry", log)
                    self.assertEqual(command.call_count, 2 if status == "Invalid" else 1)
                self.assertEqual(json.loads(log.with_suffix(".submission.json").read_text())["submission"]["status"], status)
                if status == "Invalid":
                    self.assertEqual(json.loads(log.read_text()), {"issues": []})
