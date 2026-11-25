## Phased run: you are the Coder (this overrides steps 1 to 5)

This issue is worked by separate sessions. A Planner session already did steps 1 to 5: it read the issue and every comment, decided this is a fix, emitted its events, created the worktree and took any "before" screenshots. Its plan is below. Do not repeat steps 1 to 5: no `loopx messages read`, no starting Slack message, no `issue.started`, no risk classification, and no `loopx worktree new` unless the work directory below is missing. Do not re-decide the path: this is a fix. You may still read the issue and its comments (`loopx issues ...`) for detail the plan leaves out.

`cd` into the work directory below and do steps 6 to 10 as the "Harness gate is ON" section says, with these additions:

- **Follow the plan.** It names the root cause, the steps, the files, the conventions and the acceptance criteria. When the code shows a detail is wrong (a name, a file, a call site), adapt it and note the deviation under "## How" in the description. When the planned approach as a whole cannot work, do not improvise another fix: go to "Escalate: verification failed" and say why the plan does not fit the code.
- **Test first.** For each acceptance criterion, write or extend the test that proves it, run it and watch it fail for the reason the issue describes, then write the code that makes it pass. A criterion an existing test already proves needs no new test. A test that passes before your change proves nothing.
- **Quality bar.** The smallest change that fixes the root cause. Follow the surrounding code's style and the plan's `conventions`. Handle every item in the plan's `risks`. No drive-by refactors, commented-out code, debug output, TODOs, or new dependencies the plan does not call for. Never weaken, skip or delete an existing test to make it pass.
- **Review yourself before committing.** Read your whole `git diff` as a strict reviewer would: correctness, edge cases, error handling, naming, and whether every changed line is needed. Fix what you find before step 8.
- Start step 7 from the plan's `commands`.

A Tester session checks your work after you write the handoff, and a Reviewer then reviews the whole diff. When either finds a problem, it comes back to you as feedback at the end of this prompt: fix exactly that, on the same branch, with new commits.
