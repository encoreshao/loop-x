## What changed and why

<!-- A short description of the change and the problem it solves. -->

## Verification

- [ ] `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass
- [ ] `CHANGELOG.md` has a line under `[Unreleased]` (user-visible changes only)

## Golden eval (agent instruction/tool changes only)

If this PR changes `instructions/loopx/*.md`, `loops/*/prompt.md`,
`crates/core/src/prompt.rs`, or any `allowed_tools()`, paste the
`loopx loop eval --golden` summary from the base commit and
from this branch:

<details><summary>Before</summary>

```
(paste summary)
```

</details>

<details><summary>After</summary>

```
(paste summary)
```

</details>
