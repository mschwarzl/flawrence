# Releasing

Flawrence versions follow [Semantic Versioning](https://semver.org/).
All crates in the workspace share one version.
`CHANGELOG.md` is generated from the commit subjects on `main` with [git-cliff](https://git-cliff.org).
The rules are in `cliff.toml`.

## Commit subjects

Every change reaches `main` as one squash merge per pull request.
The squash commit subject is the pull request title, so the title becomes the changelog line.
Start the title with a verb in the imperative.

| First word | Changelog section |
| --- | --- |
| Add | Added |
| Deprecate | Deprecated |
| Remove, Drop, Delete | Removed |
| Fix, Correct | Fixed |
| Security | Security |
| anything else | Changed |

## Steps

1. Install git-cliff with `cargo install --locked git-cliff`.
2. Pick the new version, for example `0.2.0`.
3. On a branch, set `version` in every `Cargo.toml` and run `cargo check` to update `Cargo.lock`.
4. Run `git cliff --unreleased --tag v0.2.0 --prepend CHANGELOG.md`.
5. Edit the new section by hand where a line reads badly. Open a pull request and merge it.
6. Tag the merge commit and push the tag.
   ```sh
   git tag -a v0.2.0 -m "Flawrence 0.2.0"
   git push origin v0.2.0
   ```
7. Create the GitHub release with the new changelog section as its notes.
   ```sh
   git cliff --latest --strip header | gh release create v0.2.0 --title "Flawrence 0.2.0" --notes-file -
   ```
