# Licence texts for crates that ship none

`scripts/third_party_notices.py --full` copies each dependency's LICENSE / COPYING / NOTICE files
out of the cargo registry. The crates below are compiled into a PuffinParse artifact on at least one
target but their published `.crate` contains no licence file, so the texts are kept here, one
directory per `<name>-<version>`. Each file was fetched unchanged from the crate's own repository at
the commit the crate was published from (the `.cargo_vcs_info.json` commit, or the release commit
when the crate carries no VCS info).

`scripts/third_party_notices.py --check` (run in CI) fails if a shipped crate has no licence text
from the registry or from here, and if a directory here no longer matches a shipped crate version,
so this list moves with `Cargo.lock`: after a dependency update, fetch the new version's files the
same way and delete the old directory.

| Crate | Licence (Cargo.toml) | Source |
|---|---|---|
| jni 0.22.4 | MIT OR Apache-2.0 | [jni-rs/jni-rs@5ae9458](https://github.com/jni-rs/jni-rs/tree/5ae9458a4ec44c5318f37ddc7569c1d4ae8a69e7) `LICENSE-MIT`, `LICENSE-APACHE` |
| jni-macros 0.22.4 | MIT OR Apache-2.0 | [jni-rs/jni-rs@33045a1](https://github.com/jni-rs/jni-rs/tree/33045a124105c939d1e2cbdcb5a39e5d868ffa03) `LICENSE-MIT`, `LICENSE-APACHE` |
| jni-sys-macros 0.4.1 | MIT OR Apache-2.0 | [jni-rs/jni-sys@64d77b7](https://github.com/jni-rs/jni-sys/tree/64d77b7a5f119d7b55b4e2c169a4668067ff59e6) `LICENSE-MIT`, `LICENSE-APACHE` |
| napi 3.13.0 | MIT | [napi-rs/napi-rs@2763e12](https://github.com/napi-rs/napi-rs/tree/2763e12efc855748485129952a6ccb97ac991c06) `LICENSE` |
| napi-derive 3.6.9 | MIT | [napi-rs/napi-rs@2763e12](https://github.com/napi-rs/napi-rs/tree/2763e12efc855748485129952a6ccb97ac991c06) `LICENSE` |
| napi-derive-backend 6.1.4 | MIT | [napi-rs/napi-rs@38162bb](https://github.com/napi-rs/napi-rs/tree/38162bb0eb324ae24b402982bad3d4ef3f24c90a) `LICENSE` |
| napi-sys 3.3.2 | MIT | [napi-rs/napi-rs@38162bb](https://github.com/napi-rs/napi-rs/tree/38162bb0eb324ae24b402982bad3d4ef3f24c90a) `LICENSE` |
| rustls-platform-verifier-android 0.1.1 | MIT OR Apache-2.0 | [rustls/rustls-platform-verifier@da3d9c3](https://github.com/rustls/rustls-platform-verifier/tree/da3d9c36f48fb9f8f97e94f132fdab67ab0fc75b) ("android-release-support: v0.1.0 -> 0.1.1") `LICENSE-MIT`, `LICENSE-APACHE` |
| valuable 0.1.1 | MIT | [tokio-rs/valuable@9efc29b](https://github.com/tokio-rs/valuable/tree/9efc29b6e58cef28f6566a47aa7e142a55fead77) `LICENSE` |
| winapi-i686-pc-windows-gnu 0.4.0 | MIT/Apache-2.0 | [retep998/winapi-rs@9497609](https://github.com/retep998/winapi-rs/tree/9497609ef44cc9bcd16cd2411c0ee6ccaf5483aa) ("Publish 0.3.4", which set the import-library crates to 0.4.0) `LICENSE-MIT`, `LICENSE-APACHE` |
| winapi-x86_64-pc-windows-gnu 0.4.0 | MIT/Apache-2.0 | same commit as above |

Not stored here: **r-efi 6.0.0** (MIT OR Apache-2.0 OR LGPL-2.1-or-later) keeps its licence
statement, the MIT and Apache-2.0 notices and its copyright lines in the `AUTHORS` file inside the
crate; the script includes that file. PuffinParse uses it under MIT. It is only compiled for UEFI
targets, which no PuffinParse artifact builds.

The napi-rs `LICENSE` file holds two MIT notices (2020-present LongYinan, 2018 GitHub); both are
reproduced.
