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

## Before a release

```sh
cargo xtask prerelease
```

That runs every test that stays `#[ignore]`d because no hosted runner has what
it needs: a real audio device, a loopback device, the open internet. It says
what each one needs before it starts and what a pass proved after, and it fails
rather than skipping when a device is missing. All but one are the only coverage
of audio content through a real device, the sharing mode the Windows backend
reports, a device producing on its own clock, and the depth the playout cushion
settles on, so run it on a machine with an interface plugged in and paste the
output into the release pull request before merging it. That pull request is the
record that the checks ran.

Two of them measure how a machine schedules the thread filling playout, and one
of those starves it on purpose, so give that run the machine to itself: no other
audio app, no build, no video call.

The loopback check wants a device whose output feeds its own input: BlackHole on
macOS, VB-CABLE on Windows, a null sink on Linux. Only a run on Windows
exercises the sharing mode, so a release worth shipping there wants one.

## Documentation

The user-facing guide lives in `site/` and is published to GitHub Pages. If a
change alters what someone sees or does, the page that describes it changes in
the same pull request. The changelog is the exception: release tooling owns it,
so never edit it by hand.

## Licence

Contributions are dual licensed under MIT and Apache-2.0, matching the
project.
