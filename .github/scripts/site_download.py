"""Put a released installer on the website, in place of the previous one.

Usage: python site_download.py INSTALLER VERSION DOCS_FOLDER

- copies INSTALLER to DOCS_FOLDER/download/, with a .sha256 file beside it;
- removes the installers of earlier versions from that folder;
- writes DOCS_FOLDER/download/latest.json describing it;
- rewrites the part of DOCS_FOLDER/index.html between the latest-installer
  markers, so the page names and links the new file.

The release workflow has already checked the installer against its checksum and
GitHub's record of where it was built; this script only places it.
"""

import hashlib
import html
import json
import shutil
import sys
from pathlib import Path

START = "<!-- latest-installer:start -->"
END = "<!-- latest-installer:end -->"
REPO = "https://github.com/ThineCraft/DriveCraft"


def size_text(n: int) -> str:
    return f"{n / (1024 * 1024):.1f} MB"


def block(version: str, name: str, size: int, sha256: str) -> str:
    v, n = html.escape(version), html.escape(name)
    return f"""{START}
      <p class="download-version">DriveCraft {v} for Windows · {size_text(size)}</p>
      <div class="cta-links"><a class="download-main" href="/download/{n}" download>Download DriveCraft {v}</a><a href="{REPO}">Get the source <span aria-hidden="true">↗</span></a></div>
      <ul class="download-facts">
        <li>Installs for your Windows account only; no administrator rights needed. <a href="/setup.html">Set up DriveCraft</a> for your organization first.</li>
        <li>Not yet signed with a code-signing certificate, so Windows asks before running it: choose <strong>More info</strong>, then <strong>Run anyway</strong>. <a href="/code-signing.html">Code signing policy</a></li>
        <li>SHA-256 <code class="download-hash">{sha256}</code></li>
        <li>The same file is on <a href="{REPO}/releases/tag/v{v}">GitHub</a>, with the record of how GitHub built it.</li>
      </ul>
      {END}"""


def place(installer: Path, version: str, docs: Path) -> dict:
    if f"_{version}_" not in installer.name:
        raise SystemExit(f"{installer.name} is not version {version}")
    data = installer.read_bytes()
    sha256 = hashlib.sha256(data).hexdigest()
    folder = docs / "download"
    folder.mkdir(parents=True, exist_ok=True)
    for old in list(folder.glob("DriveCraft_*-setup.exe")) + list(folder.glob("DriveCraft_*-setup.exe.sha256")):
        if not old.name.startswith(installer.name):
            old.unlink()
    shutil.copyfile(installer, folder / installer.name)
    (folder / f"{installer.name}.sha256").write_text(f"{sha256}  {installer.name}\n", encoding="ascii", newline="\n")
    latest = {
        "version": version,
        "file": f"/download/{installer.name}",
        "size": len(data),
        "sha256": sha256,
        "release": f"{REPO}/releases/tag/v{version}",
    }
    (folder / "latest.json").write_text(json.dumps(latest, indent=2) + "\n", encoding="utf-8", newline="\n")

    index = docs / "index.html"
    page = index.read_text(encoding="utf-8")
    start, end = page.find(START), page.find(END)
    if start < 0 or end < start:
        raise SystemExit("index.html has no latest-installer markers")
    page = page[:start] + block(version, installer.name, len(data), sha256) + page[end + len(END):]
    index.write_text(page, encoding="utf-8", newline="\n")
    return latest


if __name__ == "__main__":
    if len(sys.argv) != 4:
        raise SystemExit(__doc__)
    done = place(Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3]))
    print(json.dumps(done, indent=2))
