"""Register a requester key or decrypt a received hardware report on this computer."""
import argparse
import contextlib
import hashlib
import filecmp
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
import urllib.request
import zipfile

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
            if not current and not rotate and not pending.exists():
                saved = list(folder.glob("age1*.agekey"))
                if len(saved) > 1:
                    raise ValueError("Several identities are backed up here. Use rotate to create a fresh registration.")
                if saved:
                    os.link(saved[0], pending)
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
            if keys.find_registration(owner) != {"gist": row["id"], "key": public}:
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
        cipher = Path(temp) / "report.age"
        with cipher.open("xb") as stream:
            subprocess.run(["gh", "gist", "view", "https://gist.github.com/" + gist_id,
                            "--raw", "--filename", "report.age"], check=True, stdout=stream)
        save_report(age, identities, cipher, output)


def decrypt_args(age, identities):
    args = [str(age), "--decrypt"]
    for identity in identities:
        args.extend(["-i", str(identity)])
    return args


def save_report(age, identities, cipher, output):
    with tempfile.TemporaryDirectory(dir=output.parent) as temp:
        plain = Path(temp) / "reviewed.zip"
        with private_output(plain) as stream:
            subprocess.run([*decrypt_args(age, identities), str(cipher)], stdout=stream,
                           stdin=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=90, check=True)
        if not zipfile.is_zipfile(plain):
            raise ValueError("Decrypted report is not a ZIP archive.")
        # Publish the complete result without replacing another receipt or partial upload.
        try:
            os.link(plain, output)
        except FileExistsError:
            if output.is_symlink() or not output.is_file() or not filecmp.cmp(plain, output, shallow=False):
                raise ValueError(f"A different file already exists at {output}. Choose another --output-dir.") from None
    print(f"Reviewed diagnostics: {output}", flush=True)


# Bound data accepted unattended. Larger submissions remain retrievable by private receipt.
MAX_INBOX = 512 * 1024 * 1024


def collect_comment(age, identities, comment, armor, destination):
    plain = subprocess.check_output(decrypt_args(age, identities), input=armor.encode("ascii"),
                                    stderr=subprocess.PIPE, timeout=10)
    match = re.fullmatch(rb"([0-9a-f]{32}) ([0-9a-f]{64})", plain)
    if not match:
        raise ValueError("Invalid collection receipt.")
    gist_id, digest = (value.decode("ascii") for value in match.groups())
    gist = keys.github("gists/" + gist_id)
    if (gist["id"] != gist_id or gist["public"] is not False or gist["owner"]["id"] != comment["user"]["id"]
            or set(gist["files"]) != {"report.age"}):
        raise ValueError("The receipt must point to the comment author's unlisted report.")
    entry = gist["files"]["report.age"]
    if not 0 < entry["size"] <= keys.MAX_COLLECTED_CIPHER:
        raise ValueError("The report exceeds the automatic collection limit.")
    output = destination / f"victory-{gist_id}.zip"
    if not output.exists():
        used = sum(f.stat().st_size for f in destination.glob("victory-*.zip") if f.is_file())
        if used + entry["size"] > MAX_INBOX:
            raise ValueError("Automatic inbox full. Move reviewed reports out of the output folder.")
    if entry.get("truncated"):
        # Use the immutable GitHub file revision and never pass credentials to a raw URL.
        url = entry["raw_url"]
        if not re.fullmatch(r"https://gist\.githubusercontent\.com/[A-Za-z0-9-]+/" + gist_id
                            + r"/raw/[0-9a-f]{40}/report\.age", url):
            raise ValueError("Invalid GitHub raw report URL.")
        with urllib.request.urlopen(url, timeout=30) as stream:
            cipher = stream.read(keys.MAX_COLLECTED_CIPHER + 1)
    else:
        cipher = entry["content"].encode("ascii")
    if len(cipher) != entry["size"] or hashlib.sha256(cipher).hexdigest() != digest:
        raise ValueError("The uploaded report has changed.")
    destination.mkdir(mode=0o700, parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=destination) as temp:
        path = Path(temp) / "report.age"
        path.write_bytes(cipher)
        save_report(age, identities, path, output)


def collect(owner, folder, destination, seen):
    identities = sorted(folder.glob("age1*.agekey"))
    registration = keys.find_registration(owner)
    if not identities or not registration:
        raise ValueError("Run setup on this machine or restore the registered requester key.")
    page = 1
    with contextlib.ExitStack() as stack:
        age = None
        while True:
            comments = keys.github(f"gists/{registration['gist']}/comments?per_page=100&page={page}")
            for comment in comments:
                body = comment["body"]
                marker = (comment["id"], comment["updated_at"])
                if marker in seen or not body.startswith(keys.RECEIPT_HEADER) or len(body) > 4096:
                    continue
                if age is None:
                    age = stack.enter_context(keys.age_tool("age"))
                try:
                    collect_comment(age, identities, comment, body[len(keys.RECEIPT_HEADER):], destination)
                except (OSError, ValueError, KeyError, subprocess.SubprocessError):
                    print(f"Could not collect receipt comment {comment['id']}. The private receipt still works for a valid report.",
                          file=sys.stderr)
                else:
                    seen.add(marker)
            if len(comments) < 100:
                return
            page += 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("setup")
    commands.add_parser("rotate", help="publish a new key and retain old private keys")
    get = commands.add_parser("receive")
    get.add_argument("receipt")
    get.add_argument("--output-dir", type=Path, default=Path.home() / "uploads")
    scan = commands.add_parser("collect", help="retrieve reports whose testers opted into automatic collection")
    scan.add_argument("--output-dir", type=Path, default=Path.home() / "uploads")
    scan.add_argument("--watch", action="store_true", help="repeat every five minutes until interrupted")
    args = parser.parse_args()
    owner = keys.account(keys.github("user"))
    parent = Path(os.environ.get("LOCALAPPDATA") or os.environ.get("XDG_DATA_HOME") or Path.home() / ".local/share")
    folder = parent / "victory" / "report-inbox" / str(owner["id"])
    if args.command == "receive":
        receive(folder, args.receipt, args.output_dir.resolve())
    elif args.command == "collect":
        destination = args.output_dir.resolve()
        if args.watch:
            print(f"Checking every five minutes. Reviewed ZIPs go to {destination}. Press Ctrl+C to stop.", flush=True)
        seen = set()
        while True:
            try:
                collect(owner, folder, destination, seen)
            except (OSError, ValueError, KeyError, subprocess.SubprocessError):
                if not args.watch:
                    raise
                print("Inbox unavailable. Retrying in five minutes.", file=sys.stderr)
            if not args.watch:
                break
            time.sleep(300)
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
    except KeyboardInterrupt:
        pass
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
