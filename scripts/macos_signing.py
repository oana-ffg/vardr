# Apple credential/archive primitives adapted from Ratatoskr (MIT, Oana Goge), reviewed a658212.
"""Local Apple signing; private inputs go from gopass to consumers in memory."""
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import shutil
import stat
import subprocess
import sys
import tempfile
import tarfile
import zipfile

ROOT = Path(__file__).resolve().parent.parent
APP = "Vardr.app"


def run(*args, data=None, sensitive=False, pass_fds=(), env=None):
    result = subprocess.run([str(a) for a in args], input=data, capture_output=True,
                            pass_fds=pass_fds, env=env, cwd=ROOT)
    if result.returncode:
        detail = "" if sensitive else result.stderr.decode(errors="replace").strip()
        raise ValueError(f"{Path(args[0]).name} failed (exit {result.returncode})" +
                         (f": {detail}" if detail else "; credential output suppressed"))
    return result.stdout


def gopass(entry):
    return run("gopass", "show", "--noparsing", "--nofuzzysearch", entry, sensitive=True)


@contextmanager
def input_pipe(data):
    read_fd, write_fd = os.pipe()
    try:
        if len(data) > 4096:
            raise ValueError("Unexpectedly large credential input")
        os.write(write_fd, data)
        os.close(write_fd)
        write_fd = -1
        yield read_fd
    finally:
        os.close(read_fd)
        if write_fd >= 0:
            os.close(write_fd)


def security(*args, redactions=()):
    # Interactive mode keeps password-bearing commands on stdin, out of argv.
    result = subprocess.run(["/usr/bin/security", "-i"],
                            input=(shlex.join(map(str, args)) + "\n").encode(), capture_output=True)
    if result.returncode:
        diagnostic = result.stderr.decode(errors="replace")
        for value in redactions:
            diagnostic = diagnostic.replace(value, "[redacted]")
        error = re.search(r"(?:Sec[A-Za-z]+|CSSM[A-Z_]+): [^\r\n]+", diagnostic)
        detail = ": " + error.group() if error else "; credential output suppressed"
        raise ValueError(f"security {args[0]} failed (exit {result.returncode}){detail}")
    return result.stdout


def cleanup_preserving_error(action):
    original = sys.exc_info()[1]
    try:
        action()
    except (ValueError, OSError) as error:
        prefix = f"{original}; " if original else ""
        raise ValueError(f"{prefix}credential cleanup failed: {error}") from error


def remove_keychain(root, keychain):
    if keychain.exists():
        try:
            run("/usr/bin/security", "delete-keychain", keychain)
        except (ValueError, OSError) as error:
            try:
                run("/usr/bin/security", "lock-keychain", keychain)
            except (ValueError, OSError) as lock_error:
                raise ValueError(f"{error}; could not lock retained keychain: {lock_error}") from error
            raise
        if keychain.exists():
            run("/usr/bin/security", "lock-keychain", keychain)
            raise ValueError(f"Signing keychain remains after deletion: {keychain}")
    shutil.rmtree(root)


def public_key(data, *args):
    return run("openssl", "pkey", *args, "-pubout", "-outform", "DER", data=data, sensitive=True)


@contextmanager
def signing_identity(certificate, key_entry):
    key = gopass(key_entry)
    pem = run("openssl", "x509", "-inform", "DER", "-in", certificate, "-outform", "PEM")
    cert_public = run("openssl", "x509", "-pubkey", "-noout", data=pem)
    if public_key(key) != public_key(cert_public, "-pubin"):
        raise ValueError("Certificate and gopass private key do not match")
    run("/usr/bin/security", "verify-cert", "-c", certificate, "-p", "codeSign")
    fingerprint = run("openssl", "x509", "-inform", "DER", "-in", certificate,
                      "-noout", "-fingerprint", "-sha1").decode().strip().split("=", 1)[1].replace(":", "")
    password = secrets.token_urlsafe(48).encode()
    keychain_password = secrets.token_urlsafe(48)
    with input_pipe(key) as key_fd, input_pipe(password + b"\n") as password_fd:
        # Keychain's PKCS#12 importer requires these legacy transport algorithms.
        # This short-lived bundle has an independent random 384-bit password.
        bundle = run("openssl", "pkcs12", "-export", "-macalg", "sha1",
                     "-keypbe", "PBE-SHA1-3DES", "-certpbe", "PBE-SHA1-3DES", "-inkey", f"/dev/fd/{key_fd}",
                     "-passout", f"fd:{password_fd}", data=pem, sensitive=True,
                     pass_fds=(key_fd, password_fd))
    root = Path(tempfile.mkdtemp(prefix="vardr-signing-"))
    keychain = root / "publisher.keychain-db"
    try:
        encrypted_bundle = root / "publisher.p12"
        with os.fdopen(os.open(encrypted_bundle, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as file:
            file.write(bundle)
        security("create-keychain", "-p", keychain_password, keychain, redactions=(keychain_password,))
        keychain.chmod(0o600)
        security("set-keychain-settings", "-lut", "3600", keychain)
        security("unlock-keychain", "-p", keychain_password, keychain, redactions=(keychain_password,))
        security("import", encrypted_bundle, "-k", keychain, "-P", password.decode(),
                 "-T", "/usr/bin/codesign", redactions=(password.decode(),))
        security("set-key-partition-list", "-S", "apple-tool:,apple:,codesign:",
                 "-s", "-k", keychain_password, keychain, redactions=(keychain_password,))
        encrypted_bundle.unlink()
        identities = run("/usr/bin/security", "find-identity", "-v", "-p", "codesigning", keychain).decode()
        if fingerprint not in identities or "Developer ID Application:" not in identities:
            raise ValueError("Temporary keychain lacks the expected Developer ID identity")
        yield keychain, fingerprint
    finally:
        cleanup_preserving_error(lambda: remove_keychain(root, keychain))


def verify_publisher(path, identifier, team, *, runtime=True):
    if not re.fullmatch(r"[A-Z0-9]{10}", team or ""):
        raise ValueError("Invalid signing team")
    if not re.fullmatch(r"[A-Za-z0-9.-]+", identifier):
        raise ValueError("Invalid signing identifier")
    requirement = (
        f'anchor apple generic and identifier "{identifier}" and certificate leaf[subject.OU] = "{team}" '
        'and certificate 1[field.1.2.840.113635.100.6.2.6] exists '
        'and certificate leaf[field.1.2.840.113635.100.6.1.13] exists'
    )
    run("/usr/bin/codesign", "--verify", "--deep", "--strict", "--test-requirement", "=" + requirement, path)
    result = subprocess.run(["/usr/bin/codesign", "--display", "--verbose=4", str(path)], capture_output=True)
    if result.returncode:
        raise ValueError("Cannot read code signature")
    info = result.stderr.decode(errors="replace")
    if (runtime and "(runtime)" not in info) or "Timestamp=" not in info:
        raise ValueError("Release requires hardened runtime and an Apple secure timestamp")
    designated = run("/usr/bin/codesign", "--display", "--requirements", "-", path).decode()
    if "cdhash" in designated:
        raise ValueError("Publisher identity must remain stable across different builds")
    print(f"Verified Developer ID publisher and identity: {identifier}")


def accepted(result):
    if result.get("status") != "Accepted":
        raise ValueError("Apple did not accept the notarization submission")


def sign(path, identifier, team, identity, *, runtime=True):
    keychain, fingerprint = identity
    options = ["--options", "runtime", "--preserve-metadata=entitlements"] if runtime else []
    run("/usr/bin/codesign", "--force", "--sign", fingerprint, "--keychain", keychain,
        "--identifier", identifier, "--timestamp", *options, path)
    verify_publisher(path, identifier, team, runtime=runtime)


def notary_credentials(entry):
    value = json.loads(gopass(entry))
    if not re.fullmatch(r"[A-Z0-9]{10}", value.get("key_id", "")):
        raise ValueError("Invalid notarization key ID")
    if not re.fullmatch(r"[a-fA-F0-9-]{36}", value.get("issuer_id", "")):
        raise ValueError("Invalid notarization issuer ID")
    key = value.get("private_key", "").encode()
    if not key.startswith(b"-----BEGIN PRIVATE KEY-----"):
        raise ValueError("Notarization entry must contain a PKCS#8 private key")
    return value, key


def remove_notary_container(root, mount, path):
    failures = []
    try:
        path.unlink(missing_ok=True)
    except OSError as error:
        failures.append(str(error))
    if os.path.ismount(mount):
        try:
            run("/usr/bin/hdiutil", "detach", mount)
        except (ValueError, OSError) as error:
            try:
                run("/usr/bin/hdiutil", "detach", "-force", mount)
            except (ValueError, OSError) as force_error:
                failures.append(f"{error}; forced detach also failed: {force_error}")
    if not os.path.ismount(mount):
        try:
            shutil.rmtree(root)
        except OSError as error:
            failures.append(str(error))
    else:
        failures.append(f"Credential volume remains mounted: {mount}")
    if failures:
        raise ValueError("; ".join(failures))


def notary_command(entry, *args):
    metadata, key = notary_credentials(entry)
    tool = run("xcrun", "--find", "notarytool").decode().strip()
    # notarytool rejects both anonymous and named pipes. Its regular-file input
    # lives only inside a disposable AES-256 image, never on an unencrypted disk.
    root = Path(tempfile.mkdtemp(prefix="vardr-notary-"))
    image = root / "credential.dmg"
    mount = root / "mounted"
    path = mount / "credential.p8"
    password = secrets.token_urlsafe(48).encode() + b"\n"
    try:
        run("/usr/sbin/diskutil", "image", "create", "blank", "--encrypt", "--stdinpassphrase",
            "--size", "64m", "--volumeName", "VardrNotary", image, data=password, sensitive=True)
        image.chmod(0o600)
        run("/usr/sbin/diskutil", "image", "attach", "--stdinpassphrase", "--nobrowse",
            "--mountPoint", mount, image, data=password, sensitive=True)
        if not os.path.ismount(mount):
            raise ValueError("Encrypted notary volume was not mounted")
        mount.chmod(0o700)
        (mount / ".metadata_never_index").touch(mode=0o600)
        run("/usr/bin/xattr", "-w", "com.apple.metadata:com_apple_backup_excludeItem", "com.apple.backupd", mount)
        with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as file:
            file.write(key)
        return subprocess.run([tool, *map(str, args), "--key", str(path),
                               "--key-id", metadata["key_id"], "--issuer", metadata["issuer_id"]],
                              capture_output=True, cwd=ROOT)
    finally:
        cleanup_preserving_error(lambda: remove_notary_container(root, mount, path))


def notarize(path, entry, log_path):
    response = notary_command(entry, "submit", path, "--wait", "--timeout", "15m", "--output-format", "json")
    try:
        result = json.loads(response.stdout)
    except json.JSONDecodeError as error:
        raise ValueError("Apple returned no usable notarization status: " +
                         response.stderr.decode(errors="replace")) from error
    receipt = log_path.with_suffix(".submission.json")
    receipt.write_text(json.dumps({"artifact": Path(path).name,
                                   "sha256": hashlib.sha256(Path(path).read_bytes()).hexdigest(),
                                   "submission": result}, indent=2) + "\n")
    print(f"Apple submission receipt: {receipt}")
    if result.get("status") in {"Invalid", "Rejected"} and result.get("id"):
        log = notary_command(entry, "log", result["id"])
        if log.returncode:
            raise ValueError("Notarization failed and its diagnostic log could not be retrieved")
        log_path.write_bytes(log.stdout)
        print(f"Notarization diagnostics: {log_path}", file=sys.stderr)
    accepted(result)
    if response.returncode:
        raise ValueError("Notarization command failed despite its returned status")


def validate_candidate_archive(path):
    with zipfile.ZipFile(path) as archive:
        for info in archive.infolist():
            name = info.filename
            if name.startswith("/") or ".." in name.split("/") or "\\" in name:
                raise ValueError("Unsafe candidate archive path")
            if not (name == APP or name.startswith(APP + "/")):
                raise ValueError("Candidate archive contains files outside the app")
            if any(part.startswith("._") for part in name.split("/")):
                raise ValueError("Candidate archive contains AppleDouble metadata")
            if stat.S_IFMT(info.external_attr >> 16) not in {0, stat.S_IFREG, stat.S_IFDIR}:
                raise ValueError("Candidate archive contains a link or special file")


def validate_updater_member(member):
    parts = member.name.split("/")
    if (parts[0] != APP or ".." in parts or "\\" in member.name
            or any(part.startswith("._") for part in parts)
            or not (member.isdir() or member.isfile())
            or (member.name == APP and not member.isdir())):
        raise ValueError("Unsafe updater member or AppleDouble metadata")
    return member


def pack_updater(app, path):
    def portable(member):
        validate_updater_member(member)
        member.uid = member.gid = 0
        member.uname = member.gname = ""
        return member
    # Python's tar implementation does not synthesize macOS ._* metadata entries.
    with tarfile.open(path, "w:gz", format=tarfile.PAX_FORMAT) as archive:
        archive.add(app, arcname=APP, filter=portable)


def extract_updater(path, destination):
    with tarfile.open(path, "r:gz") as archive:
        for member in archive.getmembers():
            validate_updater_member(member)
        if hasattr(tarfile, "data_filter"):
            archive.extractall(destination, filter="data")
        else:
            # Python 3.9: the validation above allows only app directories/files.
            archive.extractall(destination)
