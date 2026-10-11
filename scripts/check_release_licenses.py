"""Fail unless every given release artifact carries PuffinParse's licence files.

    python scripts/check_release_licenses.py dist/*.whl dist/*.tar.gz dist/*.zip

Each artifact (wheel, sdist, CLI .tar.gz / .zip, npm .tgz) must contain LICENSE,
THIRD_PARTY_NOTICES.md and a non-empty THIRD_PARTY_LICENSES.txt (anywhere in the archive: wheels
keep them in *.dist-info/licenses/, sdists and npm tarballs under a top-level directory, the CLI
archives next to the binary). The release workflow runs it on every artifact it builds. Patterns
are expanded here too, because PowerShell (the Windows runners' default shell) passes them through
unexpanded. Standard library only.
"""

from __future__ import annotations

import glob
import sys
import tarfile
import zipfile
from pathlib import Path

REQUIRED = ("LICENSE", "THIRD_PARTY_NOTICES.md", "THIRD_PARTY_LICENSES.txt")


def members(path: Path) -> dict[str, int]:
    """Basename -> size of every regular file in the archive."""
    if path.suffix in (".whl", ".zip"):
        with zipfile.ZipFile(path) as zf:
            return {Path(i.filename).name: i.file_size for i in zf.infolist() if not i.is_dir()}
    with tarfile.open(path) as tf:
        return {Path(m.name).name: m.size for m in tf.getmembers() if m.isfile()}


def main(argv: list[str]) -> int:
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2
    paths = [Path(p) for arg in argv for p in (sorted(glob.glob(arg)) if glob.has_magic(arg) else [arg])]
    if not paths:
        print(f"no artifacts match {' '.join(argv)}", file=sys.stderr)
        return 1
    failed = False
    for path in paths:
        found = members(path)
        missing = [n for n in REQUIRED if not found.get(n)]
        if missing:
            failed = True
            print(f"{path.name}: missing or empty {', '.join(missing)}", file=sys.stderr)
        else:
            print(f"{path.name}: ok ({', '.join(REQUIRED)})")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
