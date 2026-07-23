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
* [x] Add hover states for the buttons
* [ ] 
* [x] Add the playlist componet as seen here file:///home/gall/workspace/toka/design/Toka%20Style%20Guide.dc.html and $design-compare it and then fix any discrepancies
* [x] Add a "rotate" feature (left & right rotate arrows?)
* [x] Add keyboard controls for every action (rotate, skip to next video, skip to previous video, add guidance in AGENTS.md that every feature we add should have a keyboard shortcut associated with it. 
* [x] add a speed control that can speed up and slow down playback
* [ ] add skip forward / skip backwards actions that jump a set amount (10s? 30s ? whatever you recommend, lets try something and I'll see how it feels)
* [ ] Add subtitle display if there is a subtitle file in the same folder or embedded in the video. Add subtitle on/off actions and buttons and keyboard keys
* [ ] I'm not seeing any of the play/pause/scrub controls anymore even though they were previously. See the video controls in the designs (see file:///home/gall/workspace/toka/design/Toka%20Style%20Guide.dc.html) and then do $design-compare and apply and fix any discrepancies.  Also add an integration test for each of the buttons, I would want tests to fail if the controls are missing.
* [ ] I'd like to add thumbnail display, but before implementing recommend an approach. Intuitively it seems to me that we should just display the thumbnails from the file system (so from GNOME / Nautilus). Is that a reasonable thing to do ?  The problem is that I've had experience previously with other players and search apps that display GNOME / Nautilus thumbnails and they don't trigger thumbnail generation...the only way to get those thumbnails to generate is to visit the same folder directly with the Files app - and then display them. And there wasn't a straightforward way to generate thumbnails for an entire folder tree. So in many other cases apps instead generated their own thumbnails. I'd like you to recommend an approach here before implementing.
