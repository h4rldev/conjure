# Conjure

A modern build tool and dependency manager for C and C++.

Conjure compiles and links C/C++ projects from a single `conjure.kdl` manifest,
manages their dependencies (local or remote git), and produces artifacts in a
predictable layout. It drives your existing compiler and build tools rather than
replacing them.

## Features

- One manifest (`conjure.kdl`) describing sources, compiler/linker settings,
  dependencies, profiles, and multiple co-built projects.
- Parallel compilation (Rayon) and parallel dependency fetching.
- Profiles that can override far more than flags: `type`, `link`, `cc`,
  `linker`, `standard`, `arch`, sources, includes, and dependencies.
- Dependencies from local paths or git remotes (Codeberg, GitHub, Bitbucket, or
  any URL), pinned in a lockfile and cached per project scope.
- Build-system interop: Make, CMake, Autotools, Meson, Ninja, Xmake, Just, a
  free-form command, or Conjure itself (`build: conjure`).
- Toolchain support for GCC/Clang and every GCC-cli-compatible compiler
  (including cross compilers, `zig cc`, `tcc`, MinGW-w64), plus MSVC (`cl` /
  `clang-cl`).
- Content-hashed source fingerprinting so unchanged builds are skipped.
- `compile_commands.json` generation for clangd.
- Target architecture selection (`x86_64`, `x64`, `arm64`).

## Installation

From source, with a recent Rust toolchain (edition 2024, Rust 1.95+):

```sh
cargo build --release
# binary at target/release/conjure
```

A Nix dev shell is provided for the build dependencies:

```sh
nix develop -c cargo build --release
```

Released binaries are published on the releases page for Linux (x86_64), macOS (x86_64, arm64), and Windows (MSVC and MinGW-w64, x86_64).

Git access uses the bundled libgit2, so no system git binary is required.

### Usage

```sh
conjure new myproject            # scaffold a new project
conjure init                     # add a manifest to the current directory
conjure add github owner/repo    # add a dependency
conjure lock                     # pin dependencies (offline re-pin)
conjure update [name]            # re-fetch and re-pin dependencies
conjure build                    # build
conjure build -p release         # build a profile
conjure as release -- build      # run a command under a profile
conjure compile-commands         # write compile_commands.json for clangd
```

### A minimal conjure.kdl

```kdl
project {
  name myproject
  language c
  type binary
  link dynamic
  compile {
    standard c11
    c_flags -Wall -Wextra
  }
  dependencies {
    dep {
      remote github "owner/repo"
      build cmake libdep
    }
  }
}
```

See [example/](example/) for a more complete example.

## Runtime dependencies
- A C/C++ compiler: any GCC-cli-compatible compiler, or MSVC cl on Windows.
- pkg-config (optional): resolves pkg_config entries in a manifest.
- The build tool a dependency declares (Make, CMake, etc.), if any.

## License

This project is licensed under the BSD-3 Clause License - see the [LICENSE](LICENSE) file for details.
