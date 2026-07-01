# Code-Graph Resolution — Community / SOTA Survey

**Date:** 2026-07-01
**Method:** deep-research harness — 5 search angles → 19 primary sources fetched →
90 falsifiable claims extracted → 25 verified by 3-vote adversarial verification
(24 confirmed, 1 refuted). Full run: `wf_e52f55ed-78c`.
**Question:** How do consolidated, production systems build cross-file code
relationship graphs (call graphs + symbol/reference resolution) across many
languages **without** a per-language compiler — and how mature/solved is it?
**Trigger:** our tree-sitter fuzzy resolver fans an ambiguous call out to all
same-named candidates (1/N); the unsolved case is method-call homonym ambiguity
in a Dart/Flutter monorepo (`build` has ~300 definitions). Proximity (R2.5),
import-anchoring (R4) and a fan-out ceiling only dent it. See
[the resolution roadmap](2026-06-24-code-graph-resolution-roadmap.md).

## TL;DR

The field splits into **two tiers with a boundary the industry treats as
settled**, and our graph is a mature, correct implementation of the *lower*
tier — which has a **structural ceiling we have now hit**. Pure by-name / CST
matching **cannot** resolve cross-file method calls to the right receiver type;
that is (near-tautologically) the job of the type-aware tier. More heuristics do
not break the homonym barrier. The next rung is a **different class**
(type-aware), not a better fuzzy resolver.

## The two tiers

### Tier 1 — precise / type-aware (SOTA). Nearly all need a language toolchain.

| System | Technique | Compiler-free? | Notes |
|---|---|---|---|
| **Sourcegraph SCIP** (`scip-typescript/python/java/clang`) | Per-language indexer runs the real compiler/type-checker **as a library**; emits a unique `package+version+symbol` id → direct lookup | **No** | *"There is no pattern matching… compiler-accurate."* `scip-clang` *"consumes Clang as a library… after type-checking."* |
| **Meta Glean** | Per-language indexers (C++, Hack, Python, Haskell, Flow) + LSIF/SCIP (Go, Java, Rust, TS); typed schema facts | **No** | Fact-based, not fuzzy. |
| **Joern / CPG** | Default `NoResolve` matches call→method by **name+signature** over pre-built edges; type-aware receiver resolution needs a custom `ICallResolver` | Partial | Dynamic langs *"potentially involve a type-recovery step which is expensive."* Confirms name-matching alone is insufficient. |
| **Google Kythe** | Per-language extractors emit typed facts | **No** | Same family as Glean/SCIP. |

**How Tier 1 kills homonym ambiguity:** it encodes the receiver's *type* (or a
globally-unique symbol id), so `list.add()` resolves to `List.add` directly.
Sourcegraph: precise nav *"enables accurately navigating to the correct method
overload in the correct type without false positives."*

### Tier 1½ — the one genuinely compiler-free SOTA: stack-graphs (with a caveat)

- **GitHub stack-graphs** — the academic **scope-graphs** framework (Eelco
  Visser, TU Delft). Per-language name-binding rules in a tree-sitter-graph DSL;
  resolves references by **graph path-finding + a stack of paused lookups** for
  type-directed (receiver-scoped) member resolution. **No compiler, no build, no
  per-package config.** This is the closest existing engine to our architecture
  that resolves cross-file *well* without a compiler.
- **BUT (maturity):** the repo was **archived on 2025-09-09 — "no longer
  supported or updated by GitHub."** Fork-and-own, not a live dependency. The
  claim that it powers precise nav "across millions of repos / hundreds of
  languages" was **REFUTED** in verification (GitHub ships precise nav for a
  limited language set). It also requires **per-language DSL authoring** (we have
  44 languages) and does **not** do full type inference.

### Tier 2 — fuzzy / search-based. Compiler-free. **This is our tier.**

- **Sourcegraph search-based nav** (the fallback it ships for unsupported
  languages): universal-ctags + case-sensitive word-boundary text search,
  refined only by **file-extension and top-of-file import heuristics**, no type
  inference.
- **aider repo-map**: tree-sitter tags + ranked (PageRank) name-matched graph.
- **ctags / gtags / cscope**: lexical, language-agnostic, no compiler.

**Our exact failure mode is this tier's documented, vendor-acknowledged
limitation:** *"Identically named methods appear in multiple class definitions,
leading to **false positives** with search-based code navigation"*; *"Incorrect
results occur more often for tokens with **common names** (such as Get)."*
Tellingly, **our import-anchoring (R4) is literally what Sourcegraph's fuzzy tier
does** ("filters results by imports at the top of the file"). Our heuristics are
the state of the art *of the fuzzy tier* — and the fuzzy tier has a ceiling.

## The key mental model: our fan-out is "CHA without types"

**Class Hierarchy Analysis (CHA)** resolves a virtual call by fanning out to
**all subtypes/implementers of the receiver type** — *"exactly the same
over-approximation as a fuzzy by-name resolver, but bounded by the static type
hierarchy rather than by name."* So our `build`×300 fan-out is CHA with the bound
removed. **Adding the receiver's type is precisely what re-bounds it.**

## The accuracy ceiling — bounded on BOTH ends

- **Fuzzy (ours):** ceiling = lexical name matching. **Structurally cannot**
  disambiguate homonyms. No heuristic fixes this.
- **Type-aware (SOTA):** higher, but **not 100%.** ISSTA 2024 (Samhi et al.,
  "Call Graph Soundness in Android Static Analysis," arXiv 2407.07804): 13 static
  tools missed **61% of dynamically-executed methods** on average, with a hard
  **precision-vs-soundness tradeoff** (CHA most sound / least precise). *(Caveat:
  Android-specific, inflated by framework reflection/callbacks — use
  directionally, not as a general cross-file miss rate.)* Type-awareness **raises
  precision on our exact problem but does not "solve" call graphs.**

## Maturity of our graph — blunt assessment

- **We are not immature — we are at the ceiling of a legitimate, production
  class.** The tree-sitter fuzzy graph sits in the same tier Sourcegraph ships as
  fallback, plus ctags and aider. Our layered precision tiers (own-file / class /
  import / proximity / tenant-unique), clean centrality, and honest fan-out
  ceiling are good engineering *for that tier*.
- **"It's hard to get right" is the signal that we hit the structural wall of
  the fuzzy approach**, not that we are doing it badly. Homonym method resolution
  is out of reach for by-name matching *by definition*.

## Recommendation (the decision this survey is meant to settle)

1. **Stop chasing the fuzzy ceiling with more heuristics.** Keep what exists
   (proximity / import / fan-out ceiling) — it is correct fuzzy-tier hygiene —
   but accept it will not resolve method homonyms.
2. **Adopt the industry-standard hybrid: precise where available, fuzzy
   fallback** — exactly Sourcegraph's model. We already have the infra: per-
   project LSP integration and `jdtls` in the image, and the roadmap's **R8
   (wire warm-LSP to stamp authoritative edges)**. Dart ships a language server.
   → For DOC-V2 (Dart) and the typed languages, let the **LSP resolve `.build()`
   correctly** and keep tree-sitter fuzzy as the fallback for the other ~43.
3. **Do NOT adopt stack-graphs as a dependency** (archived). Borrow the *idea*
   (scope-aware receiver lookup) only if LSP proves too heavy in the daemon.
4. **Measure our own number before investing heavily.** No head-to-head
   fuzzy-vs-SCIP benchmark on one corpus exists in the literature; the
   keep-heuristics-vs-adopt-indexer call must rest on **our** measured precision
   on a hand-labeled set of ambiguous calls (fuzzy vs import-anchored vs
   Dart-LSP).

## Open questions carried forward

- Per-language authoring cost of a Dart stack-graph ruleset (repo archived, Dart
  never in GitHub's shipped set) — only relevant if we reject the LSP path.
- A lightweight compiler-free receiver-type recovery (Joern `ICallResolver` /
  scope-graph type-directed lookup) from imports + local decls + constructor
  calls — enough to prune `build`×300 to the receiver's class **without** the
  Dart analyzer. (This is "R7-lite.")
- Running the Dart analysis server as a bounded per-project SCIP-style indexer
  for the Dart subset only — indexing-cost/latency budget in the daemon.
- Measured precision/recall of current fuzzy vs import-anchored vs Dart-LSP on a
  labeled ground-truth set.

## Sources (primary, verified)

- Sourcegraph — [search-based vs precise nav](https://docs.sourcegraph.com/code_navigation/explanations/search_based_code_navigation),
  [cross-repo navigation](https://sourcegraph.com/blog/cross-repository-code-navigation),
  [announcing SCIP](https://sourcegraph.com/blog/announcing-scip),
  [scip-clang](https://sourcegraph.com/blog/announcing-scip-clang),
  [scip-typescript](https://sourcegraph.com/blog/announcing-scip-typescript)
- GitHub stack-graphs — [repo (archived)](https://github.com/github/stack-graphs),
  [blog](https://github.blog/open-source/introducing-stack-graphs/),
  [paper (arXiv 2211.01224)](https://arxiv.org/abs/2211.01224)
- Meta [Glean](https://glean.software/) · [repo](https://github.com/facebookincubator/Glean)
- Joern — [interprocedural dataflow](https://joern.io/blog/interproc-dataflow-2024/)
- ISSTA 2024 — [Call Graph Soundness in Android Static Analysis (arXiv 2407.07804)](https://arxiv.org/pdf/2407.07804)
- aider — [repo-map](https://aider.chat/2023/10/22/repomap.html)

*One refuted claim (do not cite): stack-graphs powering precise nav "across
millions of repos in hundreds of languages" (1-2).*
