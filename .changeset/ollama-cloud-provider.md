---
"callimachus": minor
---

feat: add an Ollama Cloud provider with custom server URL support. Adds
Bearer-authenticated Ollama Cloud as a chat provider alongside a configurable custom
server URL for self-hosted or remote Ollama endpoints, unifies provider base-URL
handling, and ensures the Ollama Cloud API key is never forwarded to local or custom
servers.
