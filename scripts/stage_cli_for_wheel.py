#!/usr/bin/env python3
"""Build the `puffinparse` CLI and stage it so maturin puts it into the Python wheel.

The wheel then installs the command next to the SDK (`pip install puffinparse`), and
`uvx puffinparse mcp` / `uvx puffinparse eval view <run>` work with nothing installed beforehand,
which is how coding agents start tools (ADR-28). maturin copies everything under
`python/puffinparse.data/scripts/` into the wheel's scripts directory, which installers put on
PATH. maturin refuses a configured data directory that does not exist, so pyproject.toml does not
name it: this script adds `data = "python/puffinparse.data"` to `[tool.maturin]` when it stages
the binary (release builds only; `maturin develop` keeps building the SDK alone).

    python scripts/stage_cli_for_wheel.py                                # host target
    python scripts/stage_cli_for_wheel.py --target aarch64-apple-darwin  # cross target
    python scripts/stage_cli_for_wheel.py --clean
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPTS = ROOT / "python/puffinparse.data/scripts"
PYPROJECT = ROOT / "pyproject.toml"
DATA_LINE = 'data = "python/puffinparse.data"'


def configure_data_dir(enable: bool) -> None:
    """Add (or remove) the data directory in pyproject.toml's [tool.maturin] table."""
    text = PYPROJECT.read_text(encoding="utf-8")
    lines = [ln for ln in text.splitlines() if ln.strip() != DATA_LINE]
    if enable:
        at = lines.index("[tool.maturin]") + 1
        lines.insert(at, DATA_LINE)
    PYPROJECT.write_text("\n".join(lines) + "\n", encoding="utf-8")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--target", help="Rust target triple (default: the host)")
    ap.add_argument("--clean", action="store_true", help="remove the staged binary")
    args = ap.parse_args()
    if args.clean:
        shutil.rmtree(SCRIPTS.parent, ignore_errors=True)
        configure_data_dir(False)
        return 0

    cmd = ["cargo", "build", "--release", "--locked", "-p", "puffinparse-cli"]
    if args.target:
        if shutil.which("rustup"):
            subprocess.run(["rustup", "target", "add", args.target], check=True)
        cmd += ["--target", args.target]
    print("+", " ".join(cmd), flush=True)
    subprocess.run(cmd, check=True, cwd=ROOT)

    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    out = target_dir / (args.target or "") / "release"
    windows = (args.target or "").endswith("windows-msvc") or (not args.target and sys.platform == "win32")
    name = "puffinparse.exe" if windows else "puffinparse"
    binary = out / name
    if not binary.is_file():
        print(f"error: {binary} was not built", file=sys.stderr)
        return 1
    SCRIPTS.mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, SCRIPTS / name)
    (SCRIPTS / name).chmod(0o755)
    configure_data_dir(True)
    print(f"staged {binary} -> {(SCRIPTS / name).relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
