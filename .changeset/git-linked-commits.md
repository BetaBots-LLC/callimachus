---
"callimachus": patch
---

**Git linkage: see which commits a conversation produced.** Callimachus now infers, entirely on-device, which git commits each thread led to, by overlapping the files a thread discussed (`file_mentions`) with `git log`'s changed files inside the thread's time window (shared-file count = a confidence cue). Surfaces:

- **Desktop:** a "Produced commits" section on each thread, with a "Find produced commits" button that scans the project's `git log`.
- **`cal commits`** (run in a repo, or `cal commits <path>`): the thread-to-commit timeline, one row per commit with its linked-thread count.
- **MCP `linked_commits`**: an agent asks "which commit came out of this conversation?".

Only top-level threads are linked (subagent transcripts are skipped, since their work is attributed to the parent session), and the timeline is grouped one row per commit so a big commit doesn't flood it. No git library is bundled (it shells out to `git`), and nothing leaves the machine. Backed by a new `thread_commits` table (migration 0021).
