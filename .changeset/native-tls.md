---
"callimachus": patch
---

**Switch HTTP from rustls to native TLS (OS trust store).** `reqwest`, `genai`, and the Tauri updater now use the platform's native TLS (Security.framework on macOS, SChannel on Windows, OpenSSL on Linux) instead of rustls. This drops `aws-lc-sys`/`aws-lc-rs` from the build entirely — a C library whose link step requires `libclang_rt.osx.a`, which recent GitHub `macos-latest` Xcode images don't ship, breaking CI/release linking. Using native TLS keeps the build on `macos-latest` (matching Tauri's recommended pipeline) and uses the OS certificate store. No user-facing behavior change.
