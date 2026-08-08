# Contributing

Bug reports and pull requests are welcome. If a change is large or reshapes
something, open an issue first so the design gets settled before the code.

## Before you open a pull request

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs those on Linux, macOS, and Windows, plus the end-to-end harness,
snapshot tests of every screen, the installer scripts, and a build of the
release artifacts. All of it has to pass.

A few things the checks enforce that are easy to miss:

- Pull request titles are [Conventional
  Commits](https://www.conventionalcommits.org/en/v1.0.0/): `fix(client):
  ...`. The title becomes the commit on `main` and the changelog is generated
  from it, so a wrong one is a hole in the next release.
- A feature or a fix lands with its tests in the same change. A test must not
  agree with its own mock; verify against something real.
- Tests assert what happened, never how long it took. A count of events inside
  a fixed sleep, or a reading taken from a position in a file whose length
  depends on how fast the machine ran, passes on a quiet laptop and fails on a
  loaded runner. `scripts/check-test-timing.sh` refuses both shapes.
- Comments state a constraint the code cannot show. History belongs in the
  commit message and the pull request, not in the file.
- Changing a screen means updating its snapshot and looking at the image.
  `cargo test -p jamstream-client --test ui_snapshots` writes them to
  `target/ui-previews/`; set `UPDATE_SNAPSHOTS=1` to accept new ones.

## Documentation

The user-facing guide lives in `site/` and is published to GitHub Pages. If a
change alters what someone sees or does, the page that describes it changes in
the same pull request. The changelog is the exception: release tooling owns it,
so never edit it by hand.

## Licence

Contributions are dual licensed under MIT and Apache-2.0, matching the
project.
