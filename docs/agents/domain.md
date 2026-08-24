# Domain docs

How the engineering skills should consume this repository's domain documentation when exploring the codebase.

This is a single-context repository using root `CONTEXT.md` and `docs/adr/`.

## Before exploring

1. Read the root `CONTEXT.md`.
2. Read relevant decisions under `docs/adr/`.
3. Use the glossary's canonical terminology.
4. Surface conflicts with an existing ADR instead of silently overriding it.

If any of these files don't exist, proceed silently. Missing domain documentation is not itself an error; the `/domain-modeling` skill creates it lazily when new terminology or a hard-to-reverse decision is resolved.

## File structure

This is a single-context repository:

```text
/
├── CONTEXT.md
├── docs/adr/
│   ├── 0001-example-decision.md
│   └── 0002-another-decision.md
└── src/
```

## Use the glossary's vocabulary

When output names a domain concept, use the term as defined in `CONTEXT.md`. Don't drift to synonyms the glossary explicitly avoids.

If a required concept isn't in the glossary, reconsider the terminology or note the gap for `/domain-modeling`.

## Flag ADR conflicts

If output contradicts an existing ADR, surface it explicitly rather than silently overriding.
