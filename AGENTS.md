# Repository guidance

- Make each requested change on a dedicated branch. After the change is complete and verified, commit it, push the branch, open a pull request, and enable auto-merge so it merges after the required tests pass. Default to completing this process before starting the next change. Multiple independent tasks may run in parallel only when the model judges that their branches, worktrees, and pull request lifecycles can remain safely isolated.
- Keep unrelated or pre-existing worktree changes out of the commit.
- Use the $tdd skill / test-driven development whenever possible: add or update a failing test first, implement the smallest change that makes it pass, and then refactor while keeping the tests green.
- After completing each task, check `TODO.md` for new work. Use a fresh dedicated subagent for every new task so it starts with a clean context. Assess each set of tasks for safe parallel execution, but default to processing tasks sequentially when unsure.
- Use $diagnosing-bugs to help diagnose bugs
- Every user-facing feature must include a discoverable keyboard shortcut and automated coverage for that shortcut.
