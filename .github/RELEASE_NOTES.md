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
