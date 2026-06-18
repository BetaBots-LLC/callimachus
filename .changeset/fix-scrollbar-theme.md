---
"callimachus": patch
---

Fix native scrollbars (and other native controls) showing the wrong color in packaged builds. The app set its theme via a `.dark` class but never declared CSS `color-scheme`, so the WebView painted native scrollbars using the macOS system appearance instead of the app theme. Declaring `color-scheme: light` / `dark` ties them to the active theme.
