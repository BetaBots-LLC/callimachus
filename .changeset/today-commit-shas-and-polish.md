---
"callimachus": patch
---

Today no longer prints a commit's hash on top of its subject, and the rest of the screen was brought in line with the Project page.

- **Commits you can read.** Each commit a session produced is one line: the short hash in a pill as wide as the hash, then the subject, which is the part that shortens when the window is narrow. The pill was sized for a 7-character hash and the app stores 10, so the hash ran over the chip's edge and into the subject. Clicking the hash still copies the full one, here and on the Project page, and a failed copy on the Project page now says so.
- **One click target per session.** The title and its details are one row that opens the transcript (the separate Open button is gone), Resume shows that it is starting, and the lists use the same card, row height and text sizes as the Project page.
- **Keyboard.** The time window is one Tab stop with arrow keys between the windows. Up, Down, Home and End move between sessions from anywhere in a row, Escape closes an open panel and returns to its button, and Keep, Hide and Undo explain what they do on focus as well as on hover and keep the focus in the list while they work.
- **States.** A refresh that fails in the background leaves Today on screen with a line saying it may be out of date, instead of replacing it with an error. While another window loads, only the list dims, and Copy as standup waits for it. A window with nothing in it offers 7 days, Search and Projects, and a window with only memory to review no longer reads "0 sessions in 0 repos".
