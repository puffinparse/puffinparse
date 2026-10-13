#!/usr/bin/env python3
"""Copy the run viewer's front-end into crates/puffinparse-cli/viewer/ before `cargo publish`.

`puffinparse eval view` serves the benchmark site's viewer (ADR-28), whose sources live outside
the CLI crate. A crate published to crates.io cannot read files outside its own folder, so the
release job copies them in first (crates/puffinparse-cli/build.rs prefers the repository copy
whenever it exists, so this changes nothing for a build from a checkout).

    python scripts/vendor_viewer.py           # copy
    python scripts/vendor_viewer.py --clean   # remove the copy
"""

from __future__ import annotations

import argparse
import shutil
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEST = ROOT / "crates/puffinparse-cli/viewer"
FILES = {
    "index.html": "benchmark/site/src/index.html",
    "app.js": "benchmark/site/src/app.js",
    "styles.css": "benchmark/site/src/styles.css",
    "tokens.css": "website/assets/tokens.css",
}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--clean", action="store_true", help="remove the vendored copy")
    args = ap.parse_args()
    if args.clean:
        shutil.rmtree(DEST, ignore_errors=True)
        return 0
    DEST.mkdir(parents=True, exist_ok=True)
    for name, src in FILES.items():
        shutil.copyfile(ROOT / src, DEST / name)
        print(f"copied {src} -> {(DEST / name).relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
