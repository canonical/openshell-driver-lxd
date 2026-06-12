# Commit format

All commits must be signed off (`git commit -s`) and cryptographically signed.
See [GitHub's documentation on commit signature verification](https://docs.github.com/en/authentication/managing-commit-signature-verification).

Commit messages must follow [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <description>
```

## Type

Use one of the standard Conventional Commits types, e.g. `feat`, `fix`,
`docs`, `build`, `ci`, `refactor`, `test`, `perf`, or `chore`.

## Scope

Use a scope that reflects the primary area changed, e.g.:

- `driver`: Add XYZ
- `proto`: Update XYZ
- `deps`: Update XYZ (dependency bumps, e.g. `chore(deps): bump tonic to 0.13`)

Omit the scope for changes that don't fit a single area, such as
repo-wide docs.

Depending on complexity, large changes should be split into smaller, logical
commits to facilitate review.

## Signing and attribution

`Signed-off-by` is a Developer Certificate of Origin (DCO) certification — a
legal assertion that you wrote or have the right to submit the code. Only a
human submitter can make this certification. AI agents must not add a
`Signed-off-by` line.
