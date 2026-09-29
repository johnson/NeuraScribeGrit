# NeuraScribeGrit

A feasibility spike: a C-ABI shim over [grit](https://github.com/gitbutlerapp/grit),
the pure-Rust Git reimplementation, for the NeuraScribe iOS app.

**This is not production code.** It exists to answer one question — could grit
replace NeuraScribe's two existing Git backends (MiniGit, which binds libgit2 from
Swift, and a gix-based Rust FFI)? The findings and the reasoning are in
[`../NeuraScribe/docs/grit-feasibility.md`](../NeuraScribe/docs/grit-feasibility.md).
Short version: embeddable, not a drop-in, and not worth switching to yet.

## What is here

| Path | |
|---|---|
| `src/ops.rs` | Local Git operations assembled from grit-lib primitives |
| `src/http.rs` | The `HttpClient` seam, and the Swift `URLSession` callback bridge |
| `src/net.rs` | clone / fetch / push over smart HTTP |
| `src/lib.rs` | The C ABI Swift calls |
| `tests/differential.rs` | 13 operations compared against real `git` |
| `tests/network.rs` | 6 network operations compared against real `git` over HTTP |

grit-lib is MIT and is the only crate depended on. `grit-git` and `grit-cli` are
GPL-2.0 **binary-only** crates, so depending on them would impose obligations on
anything that links this; they are deliberately not used.

## Running the tests

The local-operation tests need nothing but a Rust toolchain and `git`:

```bash
cargo test
```

The network tests need a server, because grit's own push tests run against grit's
own test server — which only proves self-consistency. `tests/setup_fixture.sh`
builds a repository with real `git`, and `tests/git_http_server.py` serves it
through `git http-backend`:

```bash
tests/setup_fixture.sh /tmp/nsg-fix
GIT_HTTP_ROOT=/tmp/nsg-fix GIT_HTTP_USER=tester GIT_HTTP_PASS=s3cret \
  python3 tests/git_http_server.py 8099 &
cargo test --features http-ureq --test network
```

They skip with a printed notice when no server is reachable, so the suite still
runs in an offline environment.

The `http-ureq` feature exists **only for the host build**. The iOS build must not
enable it: it is how the shim hands HTTP to Swift `URLSession`, which is where
TLS should come from on iOS. That is why the shipped iOS artifact carries no TLS
library of its own.

## Building for iOS

```bash
cargo build --release --target aarch64-apple-ios
```

Produces a 14 MB stripped arm64 static library exporting 26 `grit_*` C symbols.

**Known blocker:** the artifact still references `_fork`, `_execvp` and
`_posix_spawnp`, pulled in by grit's SSH transport and hook runner. App Store
static analysis will reject it until those paths are feature-gated out, even
though this shim only ever speaks HTTPS and never reaches them.

## License

MIT, matching `grit-lib`. See `THIRD_PARTY_LICENSES.md` in the main repository.
