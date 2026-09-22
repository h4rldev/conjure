# v0.4.4

Highlights since 0.4.3.

### Failed builds keep their output

Compiler, linker, and dependency build output went through the progress display,
which drops it when stderr is not a terminal (piped, redirected, or in CI). A
failed build then showed only the generic "Failed to run ..." error, whose
"check the output above" hint pointed at nothing. Child output now bypasses the
progress display, so the real compiler or linker diagnostic always appears,
terminal or not.

# v0.4.3

Highlights since 0.4.2.

### Flags containing `=` are no longer dropped

`c_flags`/`ld_flags` entries like `-DFOO=1` or `-fsanitize=address` were parsed
by KDL as node properties and silently removed, so sanitizer flags never reached
the compiler or linker. They are now rejected at parse time with a message
telling you to quote them (`"-fsanitize=address"`), so the drop cannot go
unnoticed.

### Verbose builds

`-v` echoes every compiler, linker, pkg-config, and git command, and `-vv` also
prints the environment each one runs with. Useful for confirming that flags and
search paths actually reach the toolchain.

### Manifests are found from subdirectories

`conjure` walks up from the current directory, up to 10 levels, to find
`conjure.kdl` and runs from that project root, so build, test, add, and the rest
work from anywhere inside the tree. `conjure new` and `conjure init` still act
on the current directory.

# v0.4.2

Highlights since 0.4.1.

### Fix: a single package in `generate_pc { requires ... }`

`generate_pc { requires "zlib" }` now parses. A lone package argument was
previously rejected, since only two or more worked; the untagged `generate_pc`
block flattened a single argument into a scalar that the list field could not
read. The manifest schema and the scaffolded `profiles` comment now describe the
block form as well.

### Smaller

- Parsing tests cover the single-element form of the multi-valued manifest
  fields, so a lone argument cannot silently fail again.


# v0.4.1

Highlights since 0.4.0.

### Useful pkg-config files

Generated `.pc` files now describe the real static link closure. `Libs.private`
gains the project's link flags from `ld_flags` (`-l`, `-L`, raw `-Wl,`,
`-pthread`, and library paths; compiler-only flags like `-O2` are dropped)
alongside its dependency libraries, and declared `pkg_config` packages are
referenced as package names in `Requires.private` instead of flattened `-l`
tokens.

### Configurable `generate_pc`

`generate_pc` accepts a block as well as the `#true`/`#false` shorthand:

```
generate_pc {
  prefix "/opt/foo"
  version "1.2.3"
  description "..."
  requires "zlib"
}
```

`prefix` pins an install location (the default stays relocatable through
`${pcfiledir}`), `version` and `description` override the project's, and
`requires` adds packages to `Requires.private`. A profile's block merges field
by field over the project's.

# v0.4.0

Highlights since 0.3.3.

### Incremental builds
Sources are compiled individually and tracked by content hash plus their header
dependencies (from compiler depfiles), so editing one file only recompiles it,
and editing a header only recompiles the objects that include it. Flags,
profiles, and dependency changes still rebuild everything, as they must.

### Test targets take their own flags
`tests` entries now accept `c_flags`, `ld_flags`, and `include`, merged over the
project's for that target only — so a test can define `-DBUILD_TEST` or add an
include dir without leaking into the library.

### pkg-config files for libraries
A `type library` build now writes `<output.lib>/<profile>/pkgconfig/<artifact>.pc`
(relocatable via `${pcfiledir}`), naming/description/version from the project,
`Cflags` from `compile.include`, and `Libs`/`Libs.private` from the artifact and
its dependencies. Opt out with `generate_pc #false`, globally or per profile.

### Profile inheritance
A profile can inherit from others with `extends "base" "other"`, applied in
order before its own overrides, so platform × build-mode × sanitizer dimensions
don't need one profile per combination. Unknown names and cycles are rejected at
parse time.

### `conjure as` gets real subcommand help
`conjure as <profile> <subcommand>` now presents the same per-subcommand help as
the top-level CLI, and `conjure as <profile>` with no subcommand is a clear
error. Note `--` is no longer accepted before the subcommand: `conjure as
release build`, not `conjure as release -- build`.

### MSVC shared libraries
Shared libraries on MSVC now emit an import library (`/IMPLIB`), and consumers
link it, so a project can link against a shared conjure dependency on MSVC.

### Smaller
- `conjure new` documents the new `tests`/profile fields.
- `conjure build` and `conjure test` accept `-j`/`--threads` (alias `--jobs`) to
  override the manifest's `compile.threads` for that run, including dependency
  builds.
- Unknown fields in `conjure.kdl` are now errors instead of being silently
  ignored, so a misplaced or misspelled section fails loudly.

# v0.3.3

Highlights since 0.3.2.

### Override artifact names per profile and project

`artifact` in a profile overrides the project's final artifact, i.e the name of the library or binary built, so `artifact "my-arti
fact"` in a profile named `debug` will produce `my-artifact`.

This can be scoped globally, so if you want a project called `proj` have all artifacts to be `project`, its simply a single line 
change in artifact field.

# v0.3.0

Highlights since 0.2.0.

### Tests as first-class targets
Declare test binaries in `conjure.kdl`, globally or inside a profile:

`tests { unit { src "src/test/unit.c" } }`

`conjure test` builds each target into a binary that links the project's own library (so the project must be a library under the 
selected profile); `conjure test <name>` builds just one. Test sources stay out of the project's normal build, and `conjure compi
le-commands` now emits entries for them, so clangd resolves tests with the right flags.

### Per-project profile resolution
`conjure build -p <name>` (and `conjure as <name> -- build`) now resolves the profile against each project's own profiles. A sibl
ing that doesn't define it builds with its default and says so, instead of failing the whole run. `--no-siblings` builds only the
 root.

### Smaller
- A single progress display is shared across a build and its in-process conjure dependencies.
- Fixed artifact scoping when a project builds itself (test targets), and `conjure new` now scaffolds a commented `tests {}` sect
ion.
