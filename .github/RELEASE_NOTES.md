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
