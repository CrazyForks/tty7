# `tty7-server` release assets

Contract between the **release workflow** (which produces the assets) and the
**client installer** (`§12` of `2026-07-27-remote-workspace-design.md`, which
downloads and verifies them). Both sides must agree literally — the client
derives the asset name mechanically from `uname -sm`, with no discovery step and
no listing of the release.

## Asset names

```
tty7-server-<target-triple>
```

| Asset | Target | Linkage |
|---|---|---|
| `tty7-server-x86_64-unknown-linux-musl` | `x86_64-unknown-linux-musl` | static (`crt-static`, no interpreter) |
| `tty7-server-aarch64-unknown-linux-musl` | `aarch64-unknown-linux-musl` | static (`crt-static`, no interpreter) |
| `checksums.txt` | — | sha256 of **every** asset in the release |

**No version in the filename.** The version lives in the release tag (i.e. in the
download URL) and in the remote install path (`§12`), never in the asset name.
That keeps the `uname -sm` → filename mapping a pure function with nothing to
interpolate, and makes `…/releases/latest/download/tty7-server-<triple>` a
permanently valid "current stable server" URL.

**Static is a guarantee, not a hope.** The release job asserts it
(`.github/scripts/assert-static.sh`): the binary must report `statically linked`,
carry no ELF interpreter, and declare no `DT_NEEDED` shared libraries, or the job
fails. D10 exists so one binary runs on any distro without regard to the target
machine's glibc — a dynamically-linked build would silently break that on the
first old CentOS box, far from the change that caused it.

**Size** is roughly **6 MB** (stripped, release, x86_64). Worth knowing because
§12 requires the first-install confirmation to tell the user how much is about to
be written to their machine — quote the `Content-Length`, but this is the
expected order of magnitude.

## `uname -sm` → asset

| `uname -s` | `uname -m` | Asset |
|---|---|---|
| `Linux` | `x86_64`, `amd64` | `tty7-server-x86_64-unknown-linux-musl` |
| `Linux` | `aarch64`, `arm64`, `armv8l`, `armv8b` | `tty7-server-aarch64-unknown-linux-musl` |
| `Linux` | anything else | **unsupported** — abort with the raw `uname -sm` in the message |
| anything else | — | **unsupported** — abort |

- **Match on the exact strings, then fail.** No prefix matching, no "probably
  arm" heuristics: installing the wrong architecture produces an `Exec format
  error` far from the cause. An unknown machine string is a clean, explainable
  refusal.
- **`aarch64` is what Linux actually reports**; `arm64` is accepted because some
  container images and BSD-flavoured userlands normalise to it.
- **32-bit is deliberately absent.** No `i686`, no `armv7l`, no `riscv64` — add a
  row *and* a CI target together if that ever changes.

## Download URL

```
https://github.com/l0ng-ai/tty7/releases/download/<tag>/<asset>
```

| Client version | `<tag>` |
|---|---|
| `26.7.5` | `v26.7.5` |
| `26.7.6-nightly.20260727` | `nightly` |

The nightly channel publishes to a **single rolling `nightly` tag** whose assets
are replaced every night, so a nightly client must not ask for
`v26.7.6-nightly.20260727` — that tag does not exist. Rule: version contains
`-nightly.` → tag is `nightly`; otherwise tag is `v` + version.

## Verifying (`§16`)

`checksums.txt` is GNU coreutils `sha256sum` format — 64 lowercase hex chars, two
spaces, the bare asset filename (digests below are illustrative, not real):

```
3f786850e387550fdab836ed7e6dc881de23001b4b4d8ec3a1a0b9d5e0d5c0f1  tty7-server-x86_64-unknown-linux-musl
9e107d9d372bb6826bd81d3542a419d6f0d1b0b6c1c1c1c1c1c1c1c1c1c1c1c1  tty7-server-aarch64-unknown-linux-musl
```

1. **Fetch `checksums.txt` from the same release** as the binary. HTTPS to
   `github.com` is the trust anchor; the file is not separately signed.
2. **Find the line whose filename field equals the asset name** — exact match on
   the whole field. Do not substring-search: `tty7-server-x86_64-unknown-linux-musl`
   is a substring of nothing today, but that is an accident, not a rule.
3. **Compare hex case-insensitively** against the sha256 of the bytes actually
   downloaded.
4. **Absent line, malformed line, or mismatch → abort the install.** Do not
   retry, do not fall back to an unverified install, do not write the temp file
   through (`§17`). Report the expected and actual digests.

The digest covers the raw asset bytes, i.e. exactly what gets SFTP-put to
`~/.local/share/tty7/bin/.tty7-server-<ver>.tmp` before the `chmod 0755` +
rename.

## Where this is produced

| Workflow | Job | Note |
|---|---|---|
| `.github/workflows/release.yml` | `server-musl` → `draft-release` | tagged releases; `checksums.txt` is generated in the assemble job over all collected assets |
| `.github/workflows/nightly.yml` | `server-musl` → `publish` | same assets on the rolling `nightly` tag |
| `.github/workflows/ci.yml` | `server-musl` | compile-only guard on PRs; publishes nothing |

## The Windows build bundles one of them (WSL)

A WSL distro is **not** served from a release download. Design §12: it gets the
Linux binary the Windows client already shipped with, because the distro is on
the same machine and there is no network hop worth making.

| | |
|---|---|
| Which asset | `tty7-server-x86_64-unknown-linux-musl` only — there is no ARM64 Windows target in the matrix. Add the aarch64 one *with* that target, not before |
| Where it lands | `<dir of tty7.exe>\server\<asset>`, in both the installer and the portable zip |
| Who looks there | `daemon::install::wsl` — `BUNDLED_SUBDIR`; it also accepts `<dir of tty7.exe>\<asset>`, and `TTY7_BUNDLED_SERVER_DIR` overrides both |
| If it is missing | The build still ships (a warning, mirroring `server-musl`'s own skip-don't-fail probe). A WSL connect then fails with `MissingBundled`, naming every directory searched — it never silently falls back to downloading |

**This makes `build` depend on `server-musl`** in `release.yml` and
`nightly.yml`, so the two no longer run in parallel. The directory name is a
contract with `wsl.rs`, not a packaging detail — changing it on one side breaks
WSL on the other.
