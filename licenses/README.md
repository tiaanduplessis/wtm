# Dependency notices

Release archives include notices for the locked runtime and build dependency graph,
the Rust standard library, and musl for Linux binaries. Original notice text is retained;
package metadata records declared licenses, authors, and repositories.

Some published `objc2` family crates omit license files from their crate archives. The
four `objc2-<commit>.md` files here are copies of the upstream `LICENSE.md` at the exact
commits recorded by those crates' `.cargo_vcs_info.json` files:

- [dispatch2 0.3.1 source notice](https://github.com/madsmtm/objc2/blob/8852b424193ca41602281b3d7540d7c8ed51e49a/LICENSE.md)
- [objc2 0.6.5 source notice](https://github.com/madsmtm/objc2/blob/d7d2fa23ceaa5e6096c923b081040e5d81b3b9df/LICENSE.md)
- [framework crates 0.3.2 source notice](https://github.com/madsmtm/objc2/blob/7b1abfd750a2cacaea71d6a56ecfb83cb7de560b/LICENSE.md)
- [objc2-encode 4.1.0 source notice](https://github.com/madsmtm/objc2/blob/8d214f5477365ffcbcbb7de058c86ed9a518efb7/LICENSE.md)

`MIT-reference.txt` contains the upstream project's MIT license reference text.
`musl-COPYRIGHT` is the [musl 1.2.5 copyright notice](https://git.musl-libc.org/cgit/musl/plain/COPYRIGHT?h=v1.2.5).

If a dependency update changes a missing notice's source commit, packaging stops until
the new upstream notice has been reviewed and included. Keep these notices in source
archives and retain the generated notices when redistributing binaries. This inventory
is not a substitute for the dependencies' actual licensing terms.
