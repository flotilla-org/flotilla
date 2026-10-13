# Flotilla stdlib image layers

Each subdirectory is one crew image layer definition shipped as release data
([ADR 0062](../../docs/adr/0062-crew-images-are-ordered-layer-lists-built-once-per-identity.md)): a `layer.yaml`, a `Dockerfile.fragment`, and the context files
`layer.yaml` declares. The daemon embeds every subdirectory at compile time and
applies them at startup as `ImageLayer` definitions with `source: stdlib`,
named `stdlib-<name>-<version>-<hash>` (the hash keeps names injective);
stdlib records no longer shipped are deleted.

Projects select shared layers by name in their `image:` selection; needs,
`requires` and the agent runtime resolve by direct lookup on `provides`
(exact, `@version` or `>=minimum`), never by cover search. A
project's own `.flotilla/image/` and the installation's ops repository shadow
stdlib layers of the same name.

A project's own layer uses the same directory format. It lives at
`.flotilla/image/` in the root of the project's primary (first code)
repository, read at its default-branch head, or, for a charter-delegated
project, at `<charter path>/image/` in its charter (its named ops home). A
project defining both is refused, naming both locations. Removing the
directory removes the layer.

Context files are UTF-8 text, carried inline in the `ImageLayer` definition
(at most 256 KiB per layer) so the image identity hashes their contents
directly. Binary assets are not context files: fetch them in the fragment from
a pinned URL with a `sha256`, like any other download.
