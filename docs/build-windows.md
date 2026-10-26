# Windows build setup (self-contained GNU toolchain)

Local native Windows builds in this checkout use the toolchain under
`D:/Tools/usagehub-toolchain`. No global installs, no `PATH` changes, nothing
on `C:` was touched. At setup time (2026-10-26) the host had no
`cargo`/`rustup`, no VS Build Tools/`vswhere`; the MSVC route (multi-GB Build
Tools + SDK on `C:`) was rejected for disk cost. `C:` had ~19 GB free and `D:`
~18 GB free then; the toolchain below is ~3.0 GB on `D:`.

## Layout (measured 2026-10-26)

| Path | Size | Contents |
|---|---|---|
| `D:/Tools/usagehub-toolchain/cargo` | ~360 MB | `CARGO_HOME`: `cargo`, `rustc`, `rustup` shims + crates.io registry cache |
| `D:/Tools/usagehub-toolchain/rustup` | ~1.7 GB | `RUSTUP_HOME`: host toolchain `1.95.0-x86_64-pc-windows-gnullvm` (host == target, LLVM-native; `rust-std`/`rust-mingw` for it ship inside) + retired `1.95.0-x86_64-pc-windows-gnu` host kept as fallback, `rustfmt`, `clippy` |
| `D:/Tools/usagehub-toolchain/llvm-mingw/llvm-mingw-20261020-ucrt-x86_64` | 715 MB | llvm-mingw 20261020, UCRT, Clang 23.1.4 (`Target: x86_64-w64-windows-gnu`), `x86_64-w64-mingw32-gcc` wrapper, `llvm-windres` (GNU-compatible), `llvm-ar` |
| `D:/Tools/usagehub-toolchain/staticlibs/libunwind.a` | 92 KB | Copy of llvm-mingw's static libunwind; forces static unwind into the exe |
| `D:/Tools/usagehub-toolchain/dist/` | 194 MB | Installers (keep for reinstall; deleting saves ~194 MB): `rustup-init.exe` (12 MB, from `https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe`), `llvm-mingw-20261020-ucrt-x86_64.zip` (182 MB, from `https://github.com/mstorsjo/llvm-mingw/releases/download/20261020/llvm-mingw-20261020-ucrt-x86_64.zip`) |
| `D:/Tools/usagehub-toolchain/smoke/` | ~12 MB | Proof artifacts: `hello-slib.exe` (runs, prints `usagehub-toolchain-smoke-ok`), `hello-host.exe` + `hello-host.rs` (host-triple proof: gnullvm `rustc` links via the pinned wrapper, runs, prints `usagehub-toolchain-host-ok`, imports OS DLLs only), `t.o` (windres output) |
| `D:/Tools/usagehub-toolchain/vendor-check/` | tiny | `winresource-0.1.34` crate source, kept as evidence for windres discovery (see below) |

Official sources only: `static.rust-lang.org` (rustup) and
`github.com/mstorsjo/llvm-mingw` (toolchain). No OpenSSL needed:
`ureq` uses `native-tls-no-default`, i.e. Windows Schannel.

## Why `x86_64-pc-windows-gnullvm` for host AND target (not `windows-gnu`, not MSVC)

- Host == target. The Rust project publishes a `x86_64-pc-windows-gnullvm`
  host toolchain (`rustc`/`cargo`/`rust-std`/`rust-mingw`/`clippy`/`rustfmt`,
  all `available = true` in `channel-rust-1.95.0.toml` on
  `static.rust-lang.org`), installed into `RUSTUP_HOME` alongside the old
  `windows-gnu` host. `rust-toolchain.toml` pins the gnullvm host, so build
  scripts and proc-macros (proc-macro2, quote, …) compile for gnullvm and link
  through the same llvm-mingw wrapper as the final exe.
- Plain `x86_64-pc-windows-gnu` (host or target) does **not** link with
  llvm-mingw: rustc passes `-lgcc_eh -lgcc`, which do not exist there
  (reproduced 2026-10-26: `lld: error: unable to find library -lgcc_eh`).
  Verified the gnullvm host no longer asks for them:
  `rustc --print link-args --target x86_64-pc-windows-gnullvm` shows
  `--unwindlib=none -lunwind -lmingw32 -lmingwex -lmsvcrt` and zero `lgcc`
  entries — all resolvable inside llvm-mingw + `staticlibs/`.
- No fake/stub libs, no suppressed link errors: the fix is the genuine
  compiler-rt/unwind recipe Rust designed for LLVM-MinGW, fed by two official
  distributions (`static.rust-lang.org`, `github.com/mstorsjo/llvm-mingw`).

## Session setup (run in every new shell)

PowerShell:

```powershell
$env:RUSTUP_HOME = 'D:/Tools/usagehub-toolchain/rustup'
$env:CARGO_HOME  = 'D:/Tools/usagehub-toolchain/cargo'
$env:PATH = 'D:/Tools/usagehub-toolchain/cargo/bin;D:/Tools/usagehub-toolchain/llvm-mingw/llvm-mingw-20261020-ucrt-x86_64/bin;' + $env:PATH
```

`cargo/bin` provides `cargo`/`rustc`; llvm-mingw `bin/` provides
`llvm-windres`/`llvm-ar` for `build.rs` (plus `ar`/`dlltool` if ever needed).
The linker itself is pinned by absolute path in `.cargo/config.toml`, so it
works even without the `PATH` entry — but `PATH` is still required for windres.

`cmd.exe`:

```bat
set RUSTUP_HOME=D:/Tools/usagehub-toolchain/rustup
set CARGO_HOME=D:/Tools/usagehub-toolchain/cargo
set PATH=D:/Tools/usagehub-toolchain/cargo/bin;D:/Tools/usagehub-toolchain/llvm-mingw/llvm-mingw-20261020-ucrt-x86_64/bin;%PATH%
```

Do NOT set a global `RUSTUP_TOOLCHAIN`: `rust-toolchain.toml` already pins
`1.95.0-x86_64-pc-windows-gnullvm` (host == target).

## Verify, then build/test

```powershell
cargo --version   # cargo 1.95.0
rustc -vV         # rustc 1.95.0 (59807616e 2026-04-14), host x86_64-pc-windows-gnullvm
rustup toolchain list  # must include 1.95.0-x86_64-pc-windows-gnullvm
x86_64-w64-mingw32-gcc --version  # clang version 23.1.4 ...
llvm-windres --version            # llvm-windres, compatible with GNU windres

cargo build                 # debug; target defaults to x86_64-pc-windows-gnullvm via .cargo/config.toml
cargo build --release       # release (lto, strip, panic=abort per Cargo.toml)
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

First build downloads crates.io deps (~minutes) and runs `build.rs`
(locale/theme/font-subset generation + windres resource compile); that is
normal. Build output stays in `<repo>/target` on `D:` (default `CARGO_TARGET_DIR`).

## Repo-local config (already applied)

- `rust-toolchain.toml`: `channel = "1.95.0-x86_64-pc-windows-gnullvm"`
  (triple suffix is load-bearing: bare `1.95.0` would resolve the MSVC host
  triple, which has no linker here). No `targets = [...]` needed: host == target.
- `.cargo/config.toml`: `[build] target = "x86_64-pc-windows-gnullvm"`;
  `[target.x86_64-pc-windows-gnullvm] linker = <llvm-mingw gcc wrapper>`;
  `rustflags = ["-L", "<toolchain>/staticlibs"]`; MSVC `+crt-static` kept.

## Reinstall / update

```powershell
$env:RUSTUP_HOME = 'D:/Tools/usagehub-toolchain/rustup'
$env:CARGO_HOME  = 'D:/Tools/usagehub-toolchain/cargo'
& 'D:/Tools/usagehub-toolchain/dist/rustup-init.exe' -y --no-modify-path --profile minimal -c rustfmt -c clippy --default-toolchain 1.95.0-x86_64-pc-windows-gnullvm
& "$env:CARGO_HOME/bin/rustup.exe" toolchain install 1.95.0-x86_64-pc-windows-gnullvm --profile minimal -c rustfmt -c clippy
Copy-Item 'D:/Tools/usagehub-toolchain/llvm-mingw/llvm-mingw-20261020-ucrt-x86_64/x86_64-w64-mingw32/lib/libunwind.a' 'D:/Tools/usagehub-toolchain/staticlibs/'
```

Re-extract llvm-mingw from `dist/` if its directory is ever deleted.
To move the whole toolchain: reinstall, then update the three absolute paths
in `.cargo/config.toml` and the env block above accordingly.

## Publishing

This toolchain covers local builds only. The checked-in GitHub release workflow
still publishes the upstream asset name (`claude-code-usage-monitor.exe`) and
WinGet identifier and does not install this toolchain or the gnullvm target, so
it is not a UsageHub publish path. No UsageHub release is produced from it.
