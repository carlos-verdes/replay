---
name: context-digest
description: Build or refresh docs/agents/context-digest.md, a small map of a repository's agent context (AGENTS.md, CONTEXT.md, README, ADRs, docs/agents) that a worker loads instead of the full docs, then prove nothing load-bearing was dropped. Use when onboarding context is too large, when the docs changed and the digest is stale, or when a worker asks "where do I start reading".
---

# Context digest

A worker on this repo pays about 110K tokens to read README, CONTEXT.md and the ADRs
before it knows where a term is defined or which decision governs a module. Most of
that is tutorial prose and rationale it does not need for a ticket. The digest is a
**map, not a summary**: one line per term, decision, section and tunable, each with a
pointer to the authoritative text. A worker reads the map (a few thousand tokens), then
reads only the sections the ticket touches.

The digest never replaces `AGENTS.md`. Rules stay where Claude Code loads them. The
digest replaces the *exploratory* reading of README, CONTEXT.md and `docs/adr/`.

## Inputs and output

| Input | Role in the digest |
| --- | --- |
| `AGENTS.md` | Rule headings only, each pointing at its section. Never restate a rule. |
| `CONTEXT.md` | Every `###` term becomes one line: term, one-sentence meaning, its `_Avoid_` synonyms, the ADRs it cites. |
| `docs/adr/*.md` | One line per file: number, title, status, what supersedes or amends it. |
| `README.md` | One line per `##` and `###` heading: anchor and when a worker needs it. |
| `docs/agents/*.md` | One line per file: what per-repo fact it configures. |
| `Cargo.toml`, `*/src/lib.rs`, `tests/` | The crate and module map with file sizes, and where tests live. |
| Settings tables in README | Every tunable: env var, builder method, default. |

Output: `docs/agents/context-digest.md`, committed, with a header line recording the
git commit and the source fingerprint it was built from.

## Procedure

1. **Inventory the sources** with `scripts/digest_check.py inventory <repo>`. It prints
   every heading, term, ADR and env var the digest must mention, and the fingerprint.
   Do not build the digest from memory of the files; build it from this list.
2. **Write the digest** in the section order below. Rules for each line:
   - **Keep**: the name, the one-sentence meaning in the source's own words, the
     pointer (`file#anchor` or `file:line`), the status, and any number that is a
     contract (defaults, floors, limits, exit codes).
   - **Drop**: code samples, SQL, rationale, alternatives considered, migration
     walkthroughs, prose that explains why. The pointer carries those.
   - **Never paraphrase a rule or a decision into something weaker or stronger.** Use
     the ADR title and the glossary's first sentence; they were written to be the
     one-line form.
   - **Never invent a term or a decision.** If the source has no one-liner, quote the
     first sentence of the section.
   - An `_Avoid_` list is load-bearing: it stops a worker from drifting to a synonym.
     Keep it verbatim, shortened to the words.
3. **Add the traps section**: things that are true, not written in one place, and
   that bite a worker: wrong `gh` repo, broken links, duplicate ADR numbers, a floor
   that is a feature floor, a migration that is not online. Each trap names its source.
4. **Check** with `scripts/digest_check.py check <repo> <digest>`. It fails when any
   inventory item is missing from the digest, when a pointer names a file or anchor
   that does not exist, when the fingerprint in the header is stale, or when the digest
   is over budget (default 8,000 tokens; set `--budget`).
5. **Sample-verify** five ADR lines and five glossary lines by reading the source
   section and confirming the line says the same thing with no change of meaning.
   Record the five of each in the commit message. This is the step a script cannot do.
6. **Wire it in**: `docs/agents/domain.md` says "Before exploring, read these" and names
   `CONTEXT.md` and `docs/adr/`. Change it to name the digest first and the sources
   second. Do not `@import` the digest from `AGENTS.md`: that loads it into every
   session, including ones that never touch the domain.

## Section order

```
# Context digest: <repo>
<!-- built from <commit> fingerprint <sha256-16> on <date>; refresh: see .github/skills/context-digest -->
## Read this first        (3-6 lines: what the library is, the crates, where the rules are)
## Rules (AGENTS.md)      (one line per H2, pointer only)
## Crate and module map   (path, size, one line, tests that cover it)
## Glossary (CONTEXT.md)  (term: meaning. Avoid: a, b. ADR-n)
## Decisions (docs/adr)   (ADR-n title. status. superseded-by / amends)
## README map             (anchor: when you need it)
## Tunables               (env var | builder | default | README anchor)
## Workflow               (fmt, clippy, test, Docker, wasm, release pointer)
## Traps
```

## Freshness

The header fingerprint is the sha256 of the concatenated sources. `digest_check.py check`
fails when it no longer matches, so a stale digest cannot pass CI. Run the check in the
pre-commit hook next to `cargo fmt`, or as a CI step that only runs when a source path
changed. When it fails, re-run this skill; a refresh is usually a few lines, because the
sources change by one ADR or one README section at a time.

What a refresh must do, in order: re-run the inventory, add or change the lines the
inventory now lists, re-run the check, sample-verify the lines that changed.

## What the check cannot see

The script proves *presence*: every term, ADR, heading and env var is mentioned, every
pointer resolves, the budget holds. It cannot prove a one-liner is *faithful*. That is
step 5, and it is why the digest quotes the sources' own one-line forms instead of
writing new ones.
