# LLM Wiki Schema: CivGame

This document defines the structure, conventions, and workflows for maintaining the CivGame project wiki.

## Directory Structure

- `/wiki/index.md`: The content-oriented catalog.
- `/wiki/log.md`: The chronological, append-only record of wiki operations.
- `/wiki/schema.md`: This file (the instruction manual).
- `/wiki/sources/`: Raw, immutable source documents (articles, reports, datasets).
- `/wiki/pages/`: LLM-generated markdown pages (summaries, entity pages, concept pages).

## Roles & Responsibilities

- **The LLM (You)**: The primary maintainer. You create, update, and interlink wiki pages. You are responsible for keeping the wiki consistent and current.
- **The Human**: The curator and director. They provide new sources, ask questions, and guide the synthesis.

## Workflows

### Ingest
When a new source is provided or identified:
1.  **Read**: Analyze the source (either from `wiki/sources/` or the project codebase).
2.  **Synthesis**: Identify key entities, concepts, and architectural details.
3.  **Update Pages**: 
    - Create a new summary page in `wiki/pages/` for the source if appropriate.
    - Update existing entity or concept pages in `wiki/pages/` with new information.
    - Create new entity/concept pages if they don't exist.
4.  **Cross-Reference**: Ensure all new/updated pages are interlinked using relative markdown links (e.g., `[Entity](wiki/pages/entity.md)`).
5.  **Index**: Add new pages to `wiki/index.md`.
6.  **Log**: Append an entry to `wiki/log.md` using the format: `## [YYYY-MM-DD] ingest | Description`.

### Query
When answering complex questions:
1.  **Search**: Consult `wiki/index.md` first to find relevant pages.
2.  **Read**: Review the identified pages and relevant source code.
3.  **Synthesize**: Generate a comprehensive answer.
4.  **Preserve**: If the synthesis is valuable and non-trivial, file it as a new page in `wiki/pages/` and update the index/log.

### Lint
Periodically check wiki health:
1.  **Check Links**: Identify broken internal links.
2.  **Orphan Check**: Find pages in `wiki/pages/` that are not linked from `index.md` or other pages.
3.  **Contradictions**: Flag conflicting information between pages.
4.  **Data Gaps**: Suggest new topics or areas for research based on missing connections.

## Style Guidelines

- Use standard Markdown.
- Use relative links for internal references.
- Prefer clear, descriptive headers.
- Include a "Sources" or "References" section at the bottom of major pages.
- Use YAML frontmatter if metadata (e.g., tags, date) is required for advanced tools.
