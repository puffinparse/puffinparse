# Releasing PuffinParse

A release is one tag push. `.github/workflows/release.yml` builds everything, then publishes with
short-lived OIDC credentials (trusted publishing). No registry token is stored in the repo.

| Registry | Package(s) | Auth | GitHub environment |
|---|---|---|---|
| PyPI | `puffinparse` (abi3 wheels: manylinux_2_28 x86_64/aarch64, macOS x86_64/arm64, Windows x64; sdist) | trusted publishing | `pypi` |
| npm | `puffinparse` plus `puffinparse-linux-x64-gnu`, `puffinparse-linux-arm64-gnu`, `puffinparse-darwin-x64`, `puffinparse-darwin-arm64`, `puffinparse-win32-x64-msvc` | trusted publishing, with provenance | `npm` |
| crates.io | `puffinparse-core`, then `puffinparse-server`, then `puffinparse-cli` | trusted publishing (`rust-lang/crates-io-auth-action`) | `crates-io` |
| GHCR | `ghcr.io/ajinkyashejul/puffinparse` (linux/amd64 + linux/arm64, built natively on `ubuntu-24.04` / `ubuntu-24.04-arm` and merged into one manifest list; tags `X.Y.Z`, `X.Y`, `latest`) | `GITHUB_TOKEN` | none |
| GitHub Releases | CLI archives (Linux x64/arm64, macOS x64/arm64, Windows x64), wheels, sdist, `SHA256SUMS` | `GITHUB_TOKEN` | none |

npm and crates.io are opt-in: their jobs are skipped (not failed) until the repository variables
`PUBLISH_NPM` and `PUBLISH_CRATES` are set to `true` (Settings → Secrets and variables → Actions →
Variables), so a release can ship to PyPI, GHCR and GitHub before those registries are set up.

Every artifact carries `LICENSE`, `THIRD_PARTY_NOTICES.md` and `THIRD_PARTY_LICENSES.txt` (the full
licence text of every bundled crate, generated once per release by the `notices` job with
`scripts/third_party_notices.py --full`); `scripts/check_release_licenses.py` fails the build if
any wheel, sdist, CLI archive or npm tarball lacks them, and the image jobs check
`/usr/share/doc/puffinparse/`.

`puffinparse-python` and `puffinparse-node` are `publish = false`: they ship as the PyPI and npm
packages. Every registry job waits for every build, so a broken target publishes nothing. Each
publish step skips versions that are already up, so re-running a failed release is safe.

## Cutting a release

1. Bump the version in `Cargo.toml` (`[workspace.package]` and the `puffinparse-core` entry in
   `[workspace.dependencies]`), `crates/puffinparse-cli/Cargo.toml` (`puffinparse-server` dependency),
   `pyproject.toml`, `js/package.json` and `js/package-lock.json` (`cd js && npm version X.Y.Z
   --no-git-tag-version`). The workflow's first job fails if they disagree with each other or the tag.
2. In `CHANGELOG.md`, move the `[Unreleased]` entries into a `## [X.Y.Z] - YYYY-MM-DD` section.
3. Optional rehearsal: `gh workflow run release.yml --ref main` runs every build and the image
   build without publishing anything.
4. Tag and push: `git tag -a vX.Y.Z -m "PuffinParse X.Y.Z" && git push origin vX.Y.Z`.
5. Approve the `pypi`, `npm` and `crates-io` deployments if the environments require reviewers.
   The GitHub Release is created last, once every registry succeeded.

A tag with a hyphen (`v0.2.0-rc.1`) is marked as a pre-release and does not move the image's
`latest` tag.

## One-time setup (before the first tag)

**GitHub.** Settings → Environments: create `pypi`, `npm` and `crates-io`. For each, restrict
deployments to tags matching `v*` and, if you want a manual gate, add yourself as a required
reviewer.

**PyPI** supports registering a publisher before the project exists. pypi.org → Your account →
Publishing → Add a new pending publisher → GitHub: project name `puffinparse`, owner
`ajinkyashejul`, repository `puffinparse`, workflow `release.yml`, environment `pypi`.

**crates.io** only accepts a trusted publisher for a crate that already exists, so the first
version of each crate is published by hand:

1. crates.io → Account Settings → API Tokens: a token with scope `publish-new`, limited to
   `puffinparse-*`, with a short expiry.
2. From a clean checkout of the commit you will tag:
   `cargo publish -p puffinparse-core && cargo publish -p puffinparse-server && cargo publish -p puffinparse-cli`
   (pass the token with `cargo login` or `CARGO_REGISTRY_TOKEN`). Then revoke the token.
3. For each of the three crates: crate page → Settings → Trusted Publishing → Add → GitHub:
   repository owner `ajinkyashejul`, repository name `puffinparse`, workflow filename
   `release.yml`, environment `crates-io`.

4. Set the repository variable `PUBLISH_CRATES` to `true`.

The release job then sees those versions on crates.io and skips them; later versions publish
through OIDC alone.

**npm** also needs each package to exist before a trusted publisher can be added. Publish a
placeholder `0.0.0` of each of the six names once (`npm login` first):

```bash
for name in puffinparse puffinparse-linux-x64-gnu puffinparse-linux-arm64-gnu \
            puffinparse-darwin-x64 puffinparse-darwin-arm64 puffinparse-win32-x64-msvc; do
  dir=$(mktemp -d)
  printf '{"name":"%s","version":"0.0.0","description":"Placeholder; see https://github.com/ajinkyashejul/puffinparse","license":"MIT","repository":{"type":"git","url":"git+https://github.com/ajinkyashejul/puffinparse.git"}}\n' "$name" > "$dir/package.json"
  (cd "$dir" && npm publish --access public)
done
```

Then for each package: npmjs.com → package → Settings → Trusted Publisher → GitHub Actions:
organization or user `ajinkyashejul`, repository `puffinparse`, workflow filename `release.yml`,
environment `npm`. Under Publishing access, choose "Require two-factor authentication and disallow
tokens". Finally set the repository variable `PUBLISH_NPM` to `true`; the next tag publishes all six
packages.

**GHCR.** The first release creates the package as private. Afterwards: github.com/ajinkyashejul
→ Packages → `puffinparse` → Package settings → Change visibility → Public, and check that it is
linked to the `puffinparse` repository.
