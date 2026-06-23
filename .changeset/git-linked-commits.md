---
"callimachus": patch
---

**Git linkage: see which commits a conversation produced.** Callimachus now infers, entirely on-device, which git commits each thread led to, by overlapping the files a thread discussed (`file_mentions`) with `git log`'s changed files inside the thread's time window. The shared-file count doubles as a confidence cue. Run `cal commits` inside a repo (or `cal commits <path>`) to compute and print the thread-to-commit timeline; agents get a `linked_commits` MCP tool to answer "which commit came out of this conversation?". No git library is bundled (it shells out to `git`), and nothing is sent anywhere. Backed by a new `thread_commits` table (migration 0021).
