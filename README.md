# Conjure

The modern build-tool for C and C++.

## Features

- Build parallelization through Rayon
- Clangd compile-commands.json generation
- Dependency management
- Support for multiple build systems that aren't Conjure itself, e.g. Make, CMake, Xmake, Meson, Ninja, Autotools, etc.
- Build profiles
- Dependency caching and locking
- Source code fingerprinting
- Run commands as build profiles through Conjure's `as` subcommand

## TODOs

- [ ]  - Add Conjure as a build system
- [ ]  - Add richer error messages
- [ ]  - Add Conjure manifest validation
- [ ]  - Add support for other C/C++ compilers such as MSVC
- [ ]  - Make sub-project implementation a bit more robust
- [ ]  - Nix & overall linux package.

## Dependencies

- [clap](https://github.com/clap-rs/clap): For parsing command-line arguments
- [git2](https://github.com/rust-lang/git2-rs): For cloning and fetching dependencies
- [indicatif](https://github.com/console-rs/indicatif): For progress bars
- [kdl](https://github.com/kdl-org/kdl-rs): For parsing the project definition
- [miette](https://github.com/zkat/miette): For error handling
- [owo-colors](https://github.com/jam1garner/owo-colors): For colored output
- [rayon](https://github.com/rayon-rs/rayon): For parallelization
- [serde](https://github.com/serde-rs/serde): For serialization
- [serde-json](https://github.com/serde-rs/json): For serialization of compile-commands.json

## Runtime Dependencies

- pkg-config: For finding system dependencies that support pkg-config (Optional)
- Any GCC cli-compatible C/C++ compiler: For building C projects (other C compilers such as MSVC planned)

## Building

Build like any Rust project with `cargo build release`.

## Demo

Check out the [example project](./example) for a demo of Conjure in action

## License

This project is licensed under the BSD-3 Clause License - see the [LICENSE](LICENSE) file for details.
