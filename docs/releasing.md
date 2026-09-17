# Releasing

A release is a version tag. Pushing `vX.Y.Z` runs
`.github/workflows/release.yml`, which publishes the three crates to crates.io,
creates a GitHub Release with the archives, and pushes the image to
`ghcr.io/andriykohut/hawse`. Pull requests and pushes to `main` run the same
workflow without publishing, so a tag never runs a build for the first time.

## Cutting a release

Set the new version on three lines in the root `Cargo.toml`: `version` under
`[workspace.package]`, and the `version` of `hawse-proto` and `hawse-core` under
`[workspace.dependencies]`. Merge that in a PR, then tag the merge commit:

```sh
git switch main && git pull
git tag -a v0.2.0 -m v0.2.0
git push origin v0.2.0
```

The `version` job fails if the tag does not match the workspace version. Nothing
publishes until every build and check has passed. Then `publish-crates` runs,
followed by `github-release` and `publish-image`.

If the release changes the wire protocol, say so at the top of the release
notes. A server and client on different versions fail every data stream while
their logs look healthy.

## First release

The workflow authenticates to crates.io with trusted publishing, which is set up
on each crate's settings page, so the first version is published by hand to
create the crates:

```sh
cargo login
cargo publish --workspace
```

Then add a GitHub trusted publisher to each of `hawse-proto`, `hawse-core` and
`hawse` on crates.io: owner `andriykohut`, repository `hawse`, workflow
`release.yml`, no environment. Push the `v0.1.0` tag afterwards.
`publish-crates` finds all three crates published and skips them.

After the first image push, open the `hawse` package on GitHub and make it
public if it is private.

## When a run fails

A failure before `publish-crates` leaves nothing published. Fix it on `main` and
move the tag to the new commit:

```sh
git push --delete origin v0.2.0
git tag -d v0.2.0
git tag -a v0.2.0 -m v0.2.0
git push origin v0.2.0
```

A transient failure only needs `gh run rerun RUN_ID --failed`.

`build` fails at the notices step when a new dependency uses a license that
`about.toml` does not accept. Add it there and to `deny.toml`, which keeps the
same list.

`publish-crates` skips every crate whose version is already on crates.io, so a
rerun continues after a partial publish. A published version cannot be replaced.
For a broken one, fix `main` and release the next patch version;
`cargo yank --version X.Y.Z CRATE` keeps new projects from resolving the broken
one.

`github-release` fails if the release already exists. Delete it with
`gh release delete vX.Y.Z --yes`, which leaves the tag in place, and rerun.
`publish-image` can be rerun as it is, since pushing the same tags again
replaces them.
