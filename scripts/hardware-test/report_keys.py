"""Account-owned public registrations and pinned age tools for hardware reports."""
import contextlib
import hashlib
from html.parser import HTMLParser
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request
import zipfile

REPO = "ligmaSec/victory-royale"
REGISTRATION = "victory-royale-hardware-report-key.txt"
MAX_COLLECTED_CIPHER = 16 * 1024 * 1024
RECEIPT_HEADER = "VIctory encrypted collection receipt v1\n"
AGE_RELEASE = "v1.3.2"
AGE_PACKAGES = {
    "linux": ("linux-amd64.tar.gz", "cbe24006683f8eb669266162894b9a522a1af52f2665fbc63a4bb032ed26ac10"),
    "win32": ("windows-amd64.zip", "f48d8f8f9ebe903ab5027ed067652f2cc1db94bc206976430133b905dcd8e8c7"),
}


def github(endpoint, payload=None, method=None):
    cmd = ["gh", "api", "--hostname", "github.com", endpoint]
    if method:
        cmd += ["--method", method]
    if payload is not None:
        cmd += ["--input", "-"]
    result = subprocess.run(cmd, input=json.dumps(payload) if payload is not None else None,
                            encoding="utf-8", capture_output=True, check=True)
    return json.loads(result.stdout)


def public_gist(endpoint):
    # Actions uses an installation token for repository reads. Gists live
    # outside that installation, and public registrations need no token.
    request = urllib.request.Request("https://api.github.com/" + endpoint,
                                     headers={"Accept": "application/vnd.github+json",
                                              "User-Agent": "victory-hardware-request"})
    with urllib.request.urlopen(request, timeout=15) as response:
        return json.load(response)


class GistDirectory(HTMLParser):
    """Read public Gist links and file counts from one account's directory."""
    def __init__(self, login, page):
        super().__init__()
        self.login = login
        self.links = {}
        self.older = {f"?page={page + 1}", f"https://gist.github.com/{login}?page={page + 1}"}
        self.next_page = False
        self.href = None
        self.label = []

    def handle_starttag(self, tag, attrs):
        if tag == "a":
            self.href = dict(attrs).get("href")
            self.label = []

    def handle_data(self, data):
        if self.href:
            self.label.append(data)

    def handle_endtag(self, tag):
        if tag != "a" or not self.href:
            return
        label = "".join(self.label).strip()
        match = re.fullmatch(rf"/{re.escape(self.login)}/([0-9a-f]{{32}})", self.href)
        if match:
            self.links.setdefault(match[1], set()).add(label)
        elif self.href in self.older and label == "Older":
            self.next_page = True
        self.href = None


def public_text(url):
    request = urllib.request.Request(url, headers={"User-Agent": "victory-hardware-request"})
    with urllib.request.urlopen(request, timeout=15) as response:
        return response.read().decode("utf-8")


def directory_registration(owner):
    login = owner["login"]
    found = []
    page = 1
    while True:
        directory = GistDirectory(login, page)
        directory.feed(public_text(f"https://gist.github.com/{login}?page={page}"))
        for gid, labels in directory.links.items():
            if "1 file" in labels and REGISTRATION not in labels:
                continue
            # The profile supplies candidates only. GitHub's embed identifies
            # the Gist owner, visibility and complete file list independently.
            meta = json.loads(public_text(f"https://gist.github.com/{gid}.json"))
            if meta["owner"] == login and meta["public"] is True and REGISTRATION in meta["files"]:
                if gid not in found:
                    found.append(gid)
        if not directory.next_page:
            break
        page += 1
    if len(found) > 1:
        raise ValueError("Multiple public report keys are registered. Keep one registration.")
    if not found:
        return None
    gid, = found
    raw = public_text(f"https://gist.githubusercontent.com/{login}/{gid}/raw/{REGISTRATION}")
    if account(github("users/" + login)) != owner:
        raise ValueError("The public registration no longer belongs to the requester.")
    return {"gist": gid, "key": checked_key(raw)}


def checked_key(content):
    key = content.strip()
    if not re.fullmatch(r"age1[023456789acdefghjklmnpqrstuvwxyz]{58}", key):
        raise ValueError("The registration must contain one native age public key.")
    return key


def account(user):
    if (type(user["id"]) is not int or user["id"] < 1 or user.get("type") != "User"
            or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9-]{0,38}", user["login"])):
        raise ValueError("A personal GitHub account is required for private delivery.")
    return {"id": user["id"], "login": user["login"]}


def registration(owner, gist_id, *, read=None):
    if not re.fullmatch("[0-9a-f]{32}", gist_id):
        raise ValueError("Invalid key registration ID.")
    gist = (read or github)("gists/" + gist_id)
    if gist["id"] != gist_id or gist["public"] is not True or account(gist["owner"]) != owner:
        raise ValueError("The public registration does not belong to the requester.")
    entry = gist["files"].get(REGISTRATION, {})
    if entry.get("truncated"):
        raise ValueError("The public key file is truncated.")
    return {"gist": gist_id, "key": checked_key(entry.get("content", ""))}


def find_registration(owner, *, read=None):
    read = read or github
    found = []
    page = 1
    while True:
        rows = read(f"users/{owner['login']}/gists?per_page=100&page={page}")
        found.extend(row["id"] for row in rows if row["public"] and REGISTRATION in row["files"])
        if len(rows) < 100:
            break
        page += 1
    if len(found) > 1:
        raise ValueError("Multiple public report keys are registered. Keep one registration.")
    return registration(owner, found[0], read=read) if found else None


def publication(pr, actor, actor_id, comment):
    if comment:
        pattern = rf"https://github\.com/{REPO}/pull/{pr}#issuecomment-([1-9][0-9]*)"
        match = re.fullmatch(pattern, comment)
        if not match:
            raise ValueError("Use the requester's test-request comment URL on this PR.")
        row = github(f"repos/{REPO}/issues/comments/{match[1]}")
        if (row["issue_url"] != f"https://api.github.com/repos/{REPO}/issues/{pr}"
                or row["id"] != int(match[1]) or row["body"].splitlines()[:1] != ["/victory hardware-test"]):
            raise ValueError("The linked PR comment must begin with /victory hardware-test.")
        owner = account(row["user"])
    else:
        owner = account(github("users/" + actor))
        if owner["id"] != actor_id:
            raise ValueError("The dispatch actor no longer matches this GitHub account.")
    try:
        try:
            pinned = find_registration(owner, read=public_gist)
        except urllib.error.HTTPError as error:
            if error.code not in (403, 429):
                raise
            error.close()
            pinned = directory_registration(owner)
    except ValueError as error:
        print(f"Private delivery unavailable for @{owner['login']}: {error}", file=sys.stderr)
        pinned = None
    return {"requester": owner, "registration": pinned}


def verify(binding):
    owner, pinned = binding["requester"], binding["registration"]
    try:
        current = registration(owner, pinned["gist"]) if pinned else None
    except (OSError, KeyError, subprocess.SubprocessError) as error:
        raise ValueError("Could not verify the requester before upload.") from error
    if not pinned or current != pinned:
        raise ValueError("The requester's published key is unavailable or has changed. Ask for a fresh kit.")


@contextlib.contextmanager
def age_tool(tool):
    """Extract just the selected official executable after verifying the release archive."""
    with tempfile.TemporaryDirectory(prefix="victory-age-") as folder:
        work = Path(folder)
        package, checksum = AGE_PACKAGES[sys.platform]
        name = f"age-{AGE_RELEASE}-{package}"
        subprocess.run(["gh", "release", "download", AGE_RELEASE, "-R", "github.com/FiloSottile/age",
                        "-p", name, "-D", folder], check=True, capture_output=True)
        archive = work / name
        if hashlib.sha256(archive.read_bytes()).hexdigest() != checksum:
            raise ValueError("The age download failed its checksum.")
        exe = work / (tool + (".exe" if sys.platform == "win32" else ""))
        member = "age/" + exe.name
        if sys.platform == "win32":
            with zipfile.ZipFile(archive) as bundle, bundle.open(member) as src, exe.open("xb") as out:
                shutil.copyfileobj(src, out)
        else:
            with tarfile.open(archive, "r:gz") as bundle, bundle.extractfile(member) as src, exe.open("xb") as out:
                shutil.copyfileobj(src, out)
            exe.chmod(0o700)
        yield exe
