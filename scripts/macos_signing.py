"""Reject macOS release artifacts that cannot retain publisher privacy identity."""
import argparse
import re
import subprocess


def verify_publisher(path, identifier, team_id):
    if not re.fullmatch(r"[A-Z0-9]{10}", team_id):
        raise ValueError("Configure APPLE_TEAM_ID with the publisher's ten-character Apple Team ID")
    if not re.fullmatch(r"[A-Za-z0-9.-]+", identifier):
        raise ValueError("Invalid signing identifier")
    requirement = (
        f'identifier "{identifier}" and anchor apple generic '
        'and certificate 1[field.1.2.840.113635.100.6.2.6] exists '
        'and certificate leaf[field.1.2.840.113635.100.6.1.13] exists '
        f'and certificate leaf[subject.OU] = "{team_id}"'
    )
    subprocess.run(
        ["/usr/bin/codesign", "--verify", "--strict", "--test-requirement", "=" + requirement, str(path)],
        check=True, timeout=15,
    )
    designated = subprocess.check_output(
        ["/usr/bin/codesign", "--display", "--requirements", "-", str(path)],
        text=True, timeout=15,
    )
    if "cdhash" in designated:
        raise ValueError("Release identity is bound to one build; use a stable Developer ID requirement")
    print(f"Verified publisher identity: {identifier}, team {team_id}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path")
    parser.add_argument("--identifier", required=True)
    parser.add_argument("--team-id", required=True)
    args = parser.parse_args()
    verify_publisher(args.path, args.identifier, args.team_id)
