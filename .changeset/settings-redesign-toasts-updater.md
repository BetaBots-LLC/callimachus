---
"callimachus": patch
---

Settings redesigned, with notifications and an in-app update flow.

- **Settings:** a section list on the left (General, Providers, Agents, Sources, Index & storage, Knowledge, Export, Updates) and the open section on the right, instead of one long page of cards. Settings are laid out as labeled rows; every control has a real label, secrets have a show/hide toggle, paths read in monospace, and the native dropdowns are gone. "Enable in Settings" and the command palette open the right section directly.
- **Notifications:** toasts (bottom right) report outcomes you can't see in place: a key saved or removed, a folder changed, an integration installed, a reindex or semantic build finishing (with counts), a cleanup or reclaim result, and update progress.
- **Updates:** Settings › Updates shows the running version, checks on demand, shows the release notes, and installs with a progress bar. Background checks still run on launch and every few hours; a found release now arrives as a toast with an Install button, and the first launch after an update says what changed. The command palette gained "Check for updates".
- **Fixed:** the "Delete old threads" table in Settings stayed empty after loading (the list is seeded into a form, and the form library's array field no longer re-renders on a reset), and its age filter showed the raw value instead of its label.
- **Development:** `pnpm desktop:dev` now runs the dev app against its own `index.dev.db` instead of the installed app's index. Opening the real index with a dev build upgrades it in place, after which the installed app can no longer read it; that is what made 0.11.0 crash at launch. A browser mock (`mock.html`) lets the UI run without the Rust backend for screenshots and UI work.
