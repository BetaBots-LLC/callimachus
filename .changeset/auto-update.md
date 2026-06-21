---
"callimachus": patch
---

**Auto-update now actually runs.** The updater plugin, signing, and `latest.json` endpoint were all configured, but nothing in the app ever checked for updates, so installed builds never updated themselves. The app now checks on startup and, when a newer signed release is available, shows an "Update available" prompt; one click downloads + installs it (with a progress bar) and relaunches. Implemented per the Tauri v2 updater guide (`check()` → `update.downloadAndInstall(...)` → `relaunch()`); the check fails silently offline or in dev builds. Added the `process:allow-restart` capability and the explicit Windows `passive` install mode.
