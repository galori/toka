* Merge / or bring in / from the tails branch:
    * [x] libmpv
    * [x] package.json build command like "build:mac" and "build:linux". The linux version should result in the toka app actually appearing in the apps list and to allow it to be launched from there.
    * [x] make plocate the default search provider on linux
* Improve dev process:
    * [x] Create a github workflow to run two build steps in each PR: unit tests and integration tests
    * [x] add guidance in AGENTS.md to always create a branch and a PR for changes, set the PR to auto-merge so that it automatically will get merged once the tests pass
    * [x] Ask user (me, @galori) to configure this repo to protect the main branch so PRs are required going forward
* [x] Launch error: see .scratch/launch-error.txt
* [x] Apply new design: see /home/gall/workspace/toka/design/*
* [x] Add guidance in AGENTS.md to check the TODO.md file after every task is complete to see if there are any new tasks. If there are then spawn a subagent to do each task (just so that the subagent starts with a relatively empty context). If there are multiple tasks and if it seems like the tasks could be performed in parallel then spawn the sub agents in parallel. But if unsure then do it sequentially. (The default should be sequential but assess each set of tasks to determine)
* [x] Review the designs in the designs/ folder and do a visual design review, to ensure that the app matches the design. I already saw a couple of problems: the text inside the search field ("Search videos") isn't visually centered, the magnifying glass is too small. The text on the button (like Play, Pause) arent' vertically centered
* [x] Add guidance in AGENTS.md to guide that we should do TDD development whenever possible (and do that now too)
* [x] Add a "full screen" button
* [x] Add a "loop video" / "loop playlist" button
* [ ] Add hover states for the buttons
