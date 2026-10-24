# Contributing to UUDA

Thanks for looking. The useful patches are the ones that keep the agent small, the tools typed, and a mutating call obvious.

## What lives where

| Path | Licence | What to change |
|---|---|---|
| `agent/` | Apache-2.0 | The daemon, the tools, packaging, the installer |
| `proto/` | Apache-2.0 | Wire messages. Rust types are generated from here at build time |
| `test/harness/` | Apache-2.0 | The Docker loop and the probes |
| Control plane | Proprietary, ships with Ops-Center | Not the public agent tree. See [LICENSE-PROPRIETARY](LICENSE-PROPRIETARY) |

A pull request against `agent/` or `proto/` is the path this repository reviews. Do not mix the two licences in one file.

## Build and test

You need Rust 1.88 (see `agent/rust-toolchain.toml`), `protoc`, and a C compiler.

```bash
cd agent
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

Those three commands are the agent job in `.forgejo/workflows/ci.yml`.

`cargo build` runs `agent/crates/uc-types/build.rs`, which reads `proto/uc/v1/*.proto` and writes generated Rust under the crate. Edit the `.proto` files, then rebuild. Do not hand-edit generated output, and do not renumber or reuse a field number. Reserve removed numbers. New fields go at the end. `proto/README.md` has the rest of the rules.

## Running a change

The daemon registers before it listens. A config file, a bootstrap token, and a control plane that can sign the CSR are required. `UC_AGENT_LISTEN_IP` only changes the bind address. It does not skip registration.

`agent/install.sh` needs root and a connected Tailscale. Pass `--base-url`. The script's built-in default is not a public address.

The Docker harness is `test/harness/smoke.sh`. It needs a Linux release binary and the commercial control-plane routers the compose file mounts. `test/harness/README.md` describes the stack.

## Packaged builds

- Linux: `agent/packaging/build-deb.sh --arch amd64|arm64 --binary <path>`. The package does not start the service.
- macOS: `agent/packaging/sign-macos.sh`. It needs `UC_SIGN_IDENTITY` and notary credentials, and it exits without changing the binary when those are unset.

## Secrets and hosts

Do not commit tokens, private keys, customer data, or internal host names. Examples use placeholders such as `<bootstrap-token>` and `https://<ops-center-host>`.

## Security reports

Send vulnerabilities to security@magicunicorn.tech. [SECURITY.md](SECURITY.md) has the details. A public issue is the wrong place for an unfixed vulnerability.

## Commit style

This repository uses short, scoped subjects (`fix(launchd): …`, `docs(readme): …`). Describe the behaviour you changed. The changelog in `CHANGELOG.md` is the place for a user-visible note.
