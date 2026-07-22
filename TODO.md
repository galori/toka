* Merge / or bring in / from the tails branch:
    * [x] libmpv
    * [x] package.json build command like "build:mac" and "build:linux". The linux version should result in the toka app actually appearing in the apps list and to allow it to be launched from there.
    * make plocate the default search provider on linux
* Improve dev process:
    * Create a github workflow to run two build steps in each PR: unit tests and integration tests
    * add guidance in AGENTS.md to always create a branch and a PR for changes, set the PR to auto-merge so that it automatically will get merged once the tests pass
    * Ask user (me, @galori) to configure this repo to protect the main branch so PRs are required going forward 

