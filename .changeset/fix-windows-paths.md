---
"callimachus": patch
---

fix(windows): resolve the index DB and per-agent data dirs with platform-correct
per-user paths. The DB location and every agent indexer were hardcoded to the
macOS `$HOME/Library/Application Support` layout. On Windows `$HOME` is normally
unset, so the DB path collapsed to a relative path under the install dir (e.g.
`C:\Program Files\Callimachus`), which a standard user cannot write: the app
crashed unless run as administrator, and indexing never found any agent data.
Now uses `dirs::data_local_dir` for the index and `dirs::home_dir` /
`dirs::config_dir` for agent discovery. macOS paths are byte-identical (no
migration); Windows and Linux now resolve correctly.
