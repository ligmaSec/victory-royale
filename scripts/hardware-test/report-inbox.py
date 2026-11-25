"""Register a requester key or decrypt a received hardware report on this computer."""
import argparse
import filecmp
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

import report_keys as keys


def setup(owner, folder, rotate):
    with keys.age_tool("age-keygen") as keygen:
        current = keys.find_registration(owner)
        local = folder / (current["key"] + ".agekey") if current else None
        pending = folder / "pending.agekey"
        if current and pending.exists() and local.exists() and public_key(keygen, pending) == current["key"]:
            # A previous upload completed before cleanup. Reuse the confirmed complete identity.
            if public_key(keygen, local) == current["key"]:
                pending.unlink()
        if current and not rotate:
            if not local.is_file() or public_key(keygen, local) != current["key"]:
                raise ValueError("Restore your backed-up key folder, or deliberately run rotate.")
        else:
            # age-keygen creates its output exclusively. Keep pending material across API failures.
            if not pending.exists():
                subprocess.run([str(keygen), "-o", str(pending)], check=True, stderr=subprocess.PIPE)
            public = public_key(keygen, pending)
            local = folder / (public + ".agekey")
            if not local.exists():
                os.link(pending, local)
            if public_key(keygen, local) != public:
                raise ValueError("The saved identity does not match its public key.")
            payload = {"files": {keys.REGISTRATION: {"content": public + "\n"}}}
            if current:
                row = keys.github("gists/" + current["gist"], payload, "PATCH")
            else:
                row = keys.github("gists", dict(payload, public=True))
            if keys.registration(owner, row["id"])["key"] != public:
                raise ValueError("GitHub registration verification failed.")
            pending.unlink()
        print(f"Private reports are configured for @{owner['login']}.")
        print(f"Back up this private folder in encrypted storage: {folder}")


def public_key(tool, path):
    return subprocess.check_output([str(tool), "-y", str(path)], stderr=subprocess.PIPE, text=True).strip()


def private_output(path):
    return os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb")


def receive(folder, receipt, destination):
    match = re.fullmatch(r"(?:https://gist\.github\.com/(?:[A-Za-z0-9-]+/)?)?([0-9a-f]{32})/?", receipt)
    if not match:
        raise ValueError("Use the GitHub Gist link from your notification.")
    identities = sorted(folder.glob("age1*.agekey"))
    if not identities:
        raise ValueError("Restore the requester key folder on this computer first.")
    gist_id = match[1]
    destination.mkdir(mode=0o700, parents=True, exist_ok=True)
    output = destination / f"victory-{gist_id}.zip"
    with tempfile.TemporaryDirectory(dir=destination) as temp, keys.age_tool("age") as age:
        # gh fetches the complete file, including gists above the REST inline-content limit.
        cipher = Path(temp) / "report.age"
        with cipher.open("xb") as stream:
            subprocess.run(["gh", "gist", "view", "https://gist.github.com/" + gist_id,
                            "--raw", "--filename", "report.age"], check=True, stdout=stream)
        args = [str(age), "--decrypt"]
        for identity in identities:
            args.extend(["-i", str(identity)])
        plain = Path(temp) / "reviewed.zip"
        with private_output(plain) as stream:
            subprocess.run([*args, str(cipher)], stdout=stream, stderr=subprocess.PIPE, check=True)
        # Publish the complete result without replacing another receipt or partial upload.
        try:
            os.link(plain, output)
        except FileExistsError:
            if output.is_symlink() or not output.is_file() or not filecmp.cmp(plain, output, shallow=False):
                raise ValueError(f"A different file already exists at {output}. Choose another --output-dir.") from None
    print(f"Reviewed diagnostics: {output}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("setup")
    commands.add_parser("rotate", help="publish a new key and retain old private keys")
    get = commands.add_parser("receive")
    get.add_argument("receipt")
    get.add_argument("--output-dir", type=Path, default=Path.home() / "uploads")
    args = parser.parse_args()
    owner = keys.account(keys.github("user"))
    parent = Path(os.environ.get("LOCALAPPDATA") or os.environ.get("XDG_DATA_HOME") or Path.home() / ".local/share")
    folder = parent / "victory" / "report-inbox" / str(owner["id"])
    if args.command == "receive":
        receive(folder, args.receipt, args.output_dir.resolve())
    else:
        folder.mkdir(mode=0o700, parents=True, exist_ok=True)
        lock = folder / ".setup-lock"
        try:
            lock.mkdir()
        except FileExistsError:
            raise ValueError(f"Setup is already running. After an interrupted setup, remove {lock}.") from None
        try:
            setup(owner, folder, args.command == "rotate")
        finally:
            lock.rmdir()


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
