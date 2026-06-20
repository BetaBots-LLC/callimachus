# Security & Privacy

## Privacy model

Callimachus is **local-first**. Specifically:

- Your conversation index (`index.db`) lives only on your machine and is never
  uploaded anywhere.
- Embeddings are computed **on-device** — no text is sent to a cloud embedding API.
- API keys are stored in the **OS keychain**, never in plaintext on disk or in the
  repo.
- The only outbound network traffic is to the LLM provider **you explicitly pick**
  and the one-time embedding-model download on first index. Conversation content
  leaves your machine **only** when you use a cloud provider for one of three
  features: (1) the **in-app chat**, (2) **Knowledge distillation** (`agent::distill`)
  of decisions, gotchas, and summaries, and (3) **Ask your history / RAG**
  (`agent::answer`). Choosing **local Ollama** keeps distillation and Ask fully
  on-device. Indexing, search, and embeddings are **always local** regardless of
  which engine you pick.
- The in-app agent's `run_shell` tool requires **explicit per-command approval**
  before anything executes.

## Reporting a vulnerability

Please **do not** open a public issue for security vulnerabilities.

Email **ari@shaller.dev** with:

- a description of the issue and its impact,
- steps to reproduce (a proof-of-concept if possible),
- any suggested remediation.

You'll get an acknowledgement within a few days. Once a fix is released, we're
happy to credit you (or keep you anonymous — your call).

## Supported versions

The latest released version receives security fixes. As a pre-1.0 project, fixes
ship in the next patch release rather than as backports.
