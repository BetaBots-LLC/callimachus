# Project memory: src-tauri

_Distilled by Callimachus across 1 thread(s), 0 analyzed. Project: `/Users/arishaller/betabots/callimachus/apps/desktop/src-tauri`._

# Callimachus Desktop (Tauri) – Project Memory

A Tauri desktop app for managing TODOs, knowledge, and thread distillation across a vault. Core tension: **heuristic extraction (free, day-one) vs. LLM distillation (lazy, opt-in, schema-enforced)**. Search and filtering currently cap at 500 items client-side; knowledge extraction surfaces via `/FIXME` markers and markdown file-refs.

## Key Decisions

- **Two-tier rollout**: Slice 1 = heuristic TODO extraction + `list_open_todos` (vault-wide, $0, no key). Slice 2 = LLM distillation (lazy, consent-gated, JSON schema enforced).
- **Heuristic extractor** scans for standalone word "todo" (word-boundary check) + `/FIXME` markers; no LLM cost, no privacy surface.
- **Inline markdown rendering** (`InlineMarkdown`) treats backtick code (`` `auth.rs` ``, `useMemo`, file paths) as code pills, not raw backticks; markdown file-refs (e.g. `[TodosView.tsx](path)`) render as underlined links.
- **Embedding layer**: Use existing local `bge-small` model for `vec_facts` embeddings ($0); recall mirrors existing `semantic_search_vec` pattern.
- **Source traceability**: Backend stores `source_message_id` for heuristic TODOs; facts can link back to originating message (needs `/fact` button to close sheet + scroll transcript).

## Watch Out For

- **500-item cap**: `list_open_todos` filters over loaded set (backend caps at 500). If user exceeds 500 TODOs, move filter server-side (flagged as easy follow-up).
- **Markdown file-refs are relative paths**: Links scraped from indexed dev conversations (e.g. `[TodosView.tsx](apps/desktop/...)`) are relative; may need path resolution or context.
- **Noise in extraction**: Heuristic extractor can pick up stray `[file.ts](path)` bits from indexed chats; `/FIXME` markers are cleaner but require explicit markup.
- **Knowledge tab missing**: No "Todos" tab, no "Knowledge" button on threads; extraction happens silently during indexing.
- **Chat-style anchoring**: `/knowledge` opens still bottom-anchored; may conflict with scroll-to-source UX.

## Open Threads

- **Server-side search**: Debounce search query, drop client filter, move filtering server-side for scale beyond 500.
- **Virtualized list**: If list grows huge, implement virtualization.
- **Distillation UI**: Enable in Settings (Ollama or API key) → open thread → **Distill** button. Currently gated behind opt-in.
- **Source linking**: Implement `/fact` button to close sheet and jump to originating message in transcript.
- **Stars/tags/collections**: Continuation item: `lib.rs` commands (`set_star`, `set_thread_tags`, `list_tags`) + registration (mirrors existing pattern).

## Open TODOs

- search end to end: _(thread 1141)_
- search (debounced), drop the client filter. Edit the query + remove `q`: _(thread 1141)_
- search server-side so it scales past the 500 cap. Reading the current `list_open_todos`: _(thread 1141)_
- filter is over the loaded set (backend caps `list_open_todos` at 500). If a user blows past 500 TODOs, we'd move the filter server-side — easy follow-up. Want that now, or leave it? _(thread 1141)_
- filtering, and a **virtualized** list: _(thread 1141)_
- list ever gets huge — say the word. _(thread 1141)_
- / knowledge opens still go bottom-anchored (chat-style). _(thread 1141)_
- extraction during indexing, no Todos tab, no Knowledge button on threads. Nothing surfaces. _(thread 1141)_
- /fact to where it came from**: the backend already stores `source_message_id` for heuristic todos. I can make each fact in the sheet a button that closes the sheet and scrolls the transcript to that message. It's a real feature (needs `sour… _(thread 1141)_
- tier. To see decisions/gotchas: enable distillation in Settings (Ollama or a key) → open a thread → **Distill**. _(thread 1141)_
- `/`FIXME` markers (`extractor='heuristic'`, free, no LLM). That's why some carry stray `[file.ts](path)` bits from indexed dev conversations. _(thread 1141)_
- tier, and the "links" are **markdown file-refs that got scraped into todos** (e.g. `[TodosView.tsx](apps/desktop/...)` — from our own indexed chats). My new `InlineMarkdown` renders those as underlined links, but they're relative file paths… _(thread 1141)_
- , so `` `auth.rs` `` / `useMemo` / paths get the code pill treatment instead of raw backticks. (Inline only — no full code-block pass; these are one-liners.) _(thread 1141)_
- tier is the perfect funnel: a fresh user gets instant value → sees empty "Decisions" sections → opts into distillation when they want more. No nag. _(thread 1141)_
- to jump to its thread. [TodosView.tsx](apps/desktop/src/components/TodosView.tsx) _(thread 1141)_
- `/`FIXME`, with a noise filter for code/table/JSON blobs) + `list_open_todos`. [knowledge/mod.rs](apps/desktop/src-tauri/src/knowledge/mod.rs) _(thread 1141)_
- word here)` literally contains the standalone word "todo", which *correctly* matched (4th item). The extractor's right; `mastodon` was properly rejected by the word-boundary check. Fixing the test text: _(thread 1141)_
- extractor + `list_open_todos`: _(thread 1141)_
- layer**. _(thread 1141)_
- layer first**. Smart — slice 1 is then **$0, no key, no privacy surface, usable day one**: heuristic TODO extraction + `list_open_todos` across MCP/cal/desktop. LLM distillation (lazy, with consent + schema-enforced JSON) lands in slice 2. _(thread 1141)_
- scan in the indexer → `list_open_todos` works **vault-wide, day one, no key, $0**. The LLM tier adds decisions/gotchas/summaries on top. _(thread 1141)_
- /summary, with `extractor` provenance) + a **`vec_facts`** vec0 table embedded by the **existing local bge-small** model (embedding = $0). Recall mirrors the `semantic_search_vec` you already have. Extraction = a **background job cloned fro… _(thread 1141)_
- tracker (B) is the strongest quick-win runner-up. _(thread 1141)_
- tracker** — agents constantly say "I'll do X later." Extract unfinished work across all threads → a local backlog, grouped by repo. **[M, high daily value]** _(thread 1141)_
- item ("lib.rs: set_star/set_thread_tags/list_tags commands + register") directly continuing the stars/tags/collections implementation the user approved. Example to mirror: _(thread 1141)_

