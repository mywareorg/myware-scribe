# Working agreement

## Branches

- Do the work on `dev`. `main` should only ever hold a state worth building
  from or releasing.
- One exception: rapid-fire debugging on Windows, where a build or test needs
  the branch to be pushed and pulled back and forth. Work on `main` there.
- Ask before merging `dev` into `main`. Merge at milestones and feature
  endpoints, not per commit, and say what the milestone was.

## Commits

- Imperative subject, no prefix, no trailing period, under ~72 characters.
- The body says what changed and why; call out known gaps explicitly rather
  than letting the commit imply more than it delivers.
