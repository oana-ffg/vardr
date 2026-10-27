import json
import io
from contextlib import redirect_stderr
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from macos_release import release
import finalize_release
from macos_signing import APP, extract_updater
from release_support import (hashes, require_draft, require_new_version, reviewed_commit,
                             unsigned_names, validate_hashes, validate_run)


class ReleaseBoundaries(unittest.TestCase):
    def test_ci_cannot_load_local_apple_authority(self):
        with patch.dict(os.environ, {"GITHUB_ACTIONS": "true"}), patch("macos_signing.gopass") as secret:
            with self.assertRaisesRegex(ValueError, "local Mac"):
                release(None)
            secret.assert_not_called()

    def test_exact_main_successful_dispatch_is_required(self):
        good = {"event": "workflow_dispatch", "headBranch": "main", "headSha": "reviewed",
                "conclusion": "success", "workflowName": "Build release candidates"}
        validate_run(good, "reviewed", "Build release candidates")
        for field, value in [("event", "pull_request"), ("headBranch", "feature"),
                             ("headSha", "other"), ("conclusion", "failure"), ("workflowName", "CI")]:
            with self.assertRaises(ValueError):
                validate_run({**good, field: value}, "reviewed", "Build release candidates")

    def test_missing_main_and_existing_releases_fail_before_credentials(self):
        with patch("release_support.run", side_effect=[b"", b"head", b""]):
            with self.assertRaisesRegex(ValueError, "reviewed origin/main"):
                reviewed_commit()
        with patch("release_support.version", return_value="0.4.3"):
            with patch("release_support.run", return_value=b"sha refs/tags/v0.4.3"):
                with self.assertRaisesRegex(ValueError, "already exists"):
                    require_new_version()
            with patch("release_support.run", side_effect=[b"", b'[[{"tag_name":"v0.4.3"}]]']):
                with self.assertRaisesRegex(ValueError, "including drafts"):
                    require_new_version()

    def test_published_or_different_commit_draft_cannot_be_signed(self):
        good = {"isDraft": True, "targetCommitish": "reviewed", "tagName": "v0.4.3"}
        with patch("release_support.version", return_value="0.4.3"):
            for value in [{**good, "isDraft": False}, {**good, "targetCommitish": "other"}]:
                with patch("release_support.run", return_value=json.dumps(value).encode()):
                    with self.assertRaisesRegex(ValueError, "unpublished draft"):
                        require_draft("v0.4.3", "reviewed")

    def test_all_local_hashes_are_bound_before_ci_signing(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in unsigned_names():
                (root / name).write_bytes(name.encode())
            expected = hashes(root)
            validate_hashes(root, expected)
            target = root / "vardr-linux-x86_64"
            target.write_bytes(b"tampered bytes")
            with self.assertRaisesRegex(ValueError, "bytes differ"):
                validate_hashes(root, expected)
            target.write_bytes(target.name.encode())
            (root / "credential.p8").write_bytes(b"public negative fixture")
            with self.assertRaisesRegex(ValueError, "Unexpected"):
                validate_hashes(root, expected)

    def test_tar_links_are_rejected_before_extraction(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for kind in [tarfile.SYMTYPE, tarfile.LNKTYPE]:
                archive = root / "candidate.tar.gz"
                with tarfile.open(archive, "w:gz") as tar:
                    member = tarfile.TarInfo(APP + "/link")
                    member.type = kind
                    member.linkname = "outside"
                    tar.addfile(member)
                destination = root / "extracted"
                destination.mkdir(exist_ok=True)
                with self.assertRaises(ValueError):
                    extract_updater(archive, destination)
                self.assertEqual(list(destination.iterdir()), [])

    def test_ci_signer_refuses_other_branches_or_changed_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in unsigned_names():
                (root / name).write_bytes(name.encode())
            expected = hashes(root)
            environment = {"GITHUB_ACTIONS": "true", "GITHUB_REF": "refs/heads/feature",
                           "GITHUB_SHA": "reviewed", "RELEASE_TAG": "v0.4.3",
                           "VERIFIED_HASHES": json.dumps(expected), "TAURI_SIGNING_PRIVATE_KEY": "public negative fixture"}
            argv = ["finalizer", "sign", "--directory", str(root)]
            with patch.dict(os.environ, environment), patch.object(sys, "argv", argv), patch("finalize_release.run") as consumer, redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit):
                    finalize_release.main()
                consumer.assert_not_called()
            environment["GITHUB_REF"] = "refs/heads/main"
            (root / "vardr-linux-x86_64").write_bytes(b"modified after Apple verification")
            with patch.dict(os.environ, environment), patch.object(sys, "argv", argv), patch("finalize_release.run") as consumer, redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit):
                    finalize_release.main()
                consumer.assert_not_called()

    def test_sigterm_uses_cleanup_handlers(self):
        with tempfile.TemporaryDirectory() as temporary:
            marker = Path(temporary) / "cleaned"
            program = f'''import os, signal, sys
from pathlib import Path
sys.path.insert(0, {str(Path(__file__).resolve().parents[1])!r})
import macos_release
def probe(args):
    try:
        os.kill(os.getpid(), signal.SIGTERM)
    finally:
        Path({str(marker)!r}).write_text("cleaned")
macos_release.release = probe
sys.argv = ["publisher", "--run-id", "fixture", "--certificate", "public.cer", "--signing-key-entry", "unused", "--team-id", "ABCDE12345", "--notary-entry", "unused", "--output", "unused"]
macos_release.main()
'''
            result = subprocess.run([sys.executable, "-c", program], capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(marker.read_text(), "cleaned")
            self.assertIn(b"Interrupted by signal", result.stderr)
