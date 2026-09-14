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
- Incremental builds: each object is tracked by content hash and the headers it
  includes, so only changed sources - and the objects that include a changed
  header - recompile.
- Profiles that can override far more than flags: `type`, `link`, `cc`,
  `linker`, `standard`, `arch`, sources, includes, dependencies, and the
  artifact name, and that can inherit from others with `extends`.
- Dependencies from local paths or git remotes (Codeberg, GitHub, Bitbucket, or
  any URL), pinned in a lockfile and cached per project scope.
- Build-system interop: Make, CMake, Autotools, Meson, Ninja, Xmake, Just, a
  free-form command, or Conjure itself (`build: conjure`).
- Toolchain support for GCC/Clang and every GCC-cli-compatible compiler
  (including cross compilers, `zig cc`, `tcc`, MinGW-w64), plus MSVC (`cl` /
  `clang-cl`); shared libraries emit import libraries on MSVC.
- `compile_commands.json` generation for clangd.
- pkg-config file (`.pc`) generation for libraries, relocatable and on by
  default (`generate_pc #false` to disable).
- Test targets (`tests { ... }`) built by `conjure test`; each links the
  project's library, and test sources stay out of the normal build (they also
  get their own clangd entries).
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
conjure as release build         # run a command under a profile
conjure compile-commands         # write compile_commands.json for clangd
conjure test                     # build all test targets
conjure test unit                # build one test target
conjure build --no-siblings      # skip co-built sibling projects
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

### Tests

Declare test binaries under `tests`, at the top level or inside a profile.
Each one compiles to a binary that links the project's library, so the project
must be `type library` under the selected profile.

```kdl
tests {
  unit { src "src/test/unit.c" }
}

profiles {
  lib {
    type library
    link static
    tests { integration { src "tests/integration.c" } }
  }
}
```

`conjure test` builds every target; `conjure test <name>` builds one. Test
sources are excluded from the project's own build and `conjure compile-commands`
emits entries for them.

A target can add its own `c_flags`, `ld_flags`, and `include`, merged over the
project's for that target only - so test-only defines and include dirs don't
leak into the library.

## Runtime dependencies
- A C/C++ compiler: any GCC-cli-compatible compiler, or MSVC cl on Windows.
- pkg-config (optional): resolves pkg_config entries in a manifest.
- The build tool a dependency declares (Make, CMake, etc.), if any.

## License

This project is licensed under the BSD-3 Clause License - see the [LICENSE](LICENSE) file for details.
