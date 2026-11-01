---
"callimachus": patch
---

Update the MCP server to rmcp 3.8 (same tools and handshake; 2024-11-05 through 2025-11-25 protocol versions) and the OS credential store to keyring 4 (existing saved API keys keep working; on Linux it reaches the Secret Service through pure-Rust zbus instead of libdbus).
