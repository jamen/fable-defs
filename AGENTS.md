# fable-defs — Def Compiler: Architecture & Handoff Guide

fable-defs is a **Rust** workspace providing a **from-scratch def compiler** (`defc`) that
compiles the text `.def` sources from Fable: The Lost Chapters' debug build into the retail
binary format: `game.bin`, `frontend.bin`, `script.bin`, and the shared `names.bin`. It was
extracted from the OpenAlbion engine project (`~/git/OpenAlbion`), which now consumes the
`defs` library as a downstream dependency.

**Where it stands (2026-07):** all four binaries compile entirely from text — no retail
binary is loaded at build time — and the retail engine loads and runs the output (save-load
verified). Golden byte-determinism holds and the crate is warning-clean. The **divergence
closure** phase is complete — the semantic ledger (§5) surfaces only verified genuine
artifacts (7 named + 1 sub-def `Bug`, 719 sub-def `AcceptArtifact` from extra serialized
bytes the engine discards). The focus is now on **compiler polish**: diagnostics for
modders, enum typing for type safety, and in-game modification testing.

This document is the architecture reference *and* the operational handoff. It is the
living onboarding doc — keep it current.

**Phase timeline:**
- **2026-06** — Default/type fixes: ~4,900 entries fixed (Font, PersistenceFlags, UI SetDefaults, region-script counts)
- **2026-07 hero round** — Sub-def divergences closed: CHeroExperienceDef, CAppearanceDef, CHeroMorphDef, CDegradableDef, R9 differ totality
- **2026-07 particle+weapon round** — Last hidden bugs: FlourishParticles/AugmentationParticles symbol resolution (~665 entries), WeaponTrails duplicate-key dedup (14 entries). Golden re-blessed.
- **2026-07 diagnostics round** — Warnings plumbing: unconsumed statements wired from lowered → build.rs diagnostics; bespoke-arm error propagation; strict enum/range checks; header failure surfacing; duplicate-name warnings (~1,161). 4 dead lowering functions deleted.
- **2026-07 enum typing round** — 26/29 P1 fields converted (UiDef 21 + UiStateDef 1 + 4 others). 2 LightingChannel fields confirmed false positives. 2 fields deferred (u8 WireStruct). DefIndex mistypes fixed. Golden stays green throughout.
- **Current** — In-game testing (this doc)

---

## Contents

1. [Orientation](#1-orientation)
2. [Pipeline](#2-pipeline)
3. [Central design fact: typed structs are the IR](#3-central-design-fact-typed-structs-are-the-ir)
4. [Layer guide](#4-layer-guide)
5. [Verification & the bug-fixing loop](#5-verification--the-bug-fixing-loop)
6. [The manifest](#6-the-manifest)
7. [Settled decisions](#7-settled-decisions)
8. [Divergence closure — completed](#8-divergence-closure--completed)
9. [Current roadmap](#9-current-roadmap)
10. [Operational reference](#10-operational-reference)
11. [Text layer — design notes & rationale](#11-text-layer--design-notes--rationale)
12. [Diagnostic gaps & fixes](#12-diagnostic-gaps--fixes)
13. [Enum typing](#13-enum-typing)
14. [In-game testing plan](#14-in-game-testing-plan)
15. [Release checklist](#15-release-checklist)

---

## 1. Orientation

### Workspace crates

| Crate | Type | Role |
|---|---|---|
| `defs` | lib | Format library: def text parsing, the typed def model, binary I/O (`crc32`/`bytes` primitives), semantic differ, `object.rs` mesh resolver |
| `defs-derive` | proc-macro lib | The five schema derives (`DefStruct`/`WireStruct`/`DefVariant`/`DefEnum`/`DefFlags`) the typed model is declared with (§4.2) |
| `def-compiler` | lib | Lowering: text statements → typed def structs; semantic verifier |
| `defc` | bin (`defc`) | Assembly: from-scratch builder for all four binaries; the manifest |

### Key external resources

| Resource | Path |
|---|---|
| Fable decompilation | `~/git/fable-decomp` — **absent as of 2026-07**; the `Transfer<T>` type/order oracle (§5) is unavailable until it is restored |
| OpenAlbion (upstream monorepo) | `~/git/OpenAlbion` — **absent as of 2026-07** |
| Retail binaries (ground truth) | `~/Fable/data/CompiledDefs/backup-retail-verified/` |
| Anniversary debug build (text defs) | `~/doc/Fable_Anniversary-2013-02-25/Fable/Data/` |
| Live game install (Unified Build; ships **both** header sets, §4.1.1) | `~/Fable/data/Defs/` |
| EgoCore (downstream consumer; links `def-compiler-sys`) | `~/git/EgoCore` — its `FableDefCompiler/` is a C++ reimplementation, useful as a behavioural oracle |
| In-game test artifact | `~/fable-scratch-build/` |

```bash
REF=~/Fable/data/CompiledDefs/backup-retail-verified
TEXT=~/doc/Fable_Anniversary-2013-02-25/Fable/Data

# Default build — zero flags:
cargo run -q -p defc -- $TEXT/Defs <out_dir>
```

### Output at a glance (verified state)

| Binary | Entries | Composition |
|---|---|---|
| `game.bin` | 12,604 | 249 NULLDEFs + 8,630 named + 3,725 anonymous sub-defs |
| `frontend.bin` | 810 | 9 NULLDEF entries + 801 named |
| `script.bin` | 611 | 3 NULLDEFs + 608 named |
| `names.bin` | one shared string table for all three |

---

## 2. Pipeline

```
 INPUTS                      defs (format layer)               def-compiler          defc
┌──────────────┐   ┌─────────────────────────────────────────┐   ┌──────────────────────┐   ┌─────────────────────────┐
│ Defs/*.def    │──▶│ DefParser → DefFile{ Definition{ name,   │──▶│ flatten_specialization│──▶│ build.rs: per binary     │
│ Defs/*.tpl    │   │   type, specialises, body:Vec<Statement>}│   │  (parent-first concat)│   │  • collect_named (own    │
│ headers (#def-│──▶│ HeaderParser → SymbolTable (name → i64)  │   │ lower_def dispatch:   │   │    index allocation)     │
│  ine / enum)  │   ├─────────────────────────────────────────┤   │  • ~35 bespoke arms   │   │  • NULLDEFs (def_default)│
│               │   │ ~273 DefStruct + 86 WireStruct +         │   │  • lower_generic via  │   │  • sub-def tables (game):│
│ manifest.rs   │   │ 4 DefVariant + 62 DefEnum +              │◀──│    DefBody:VisitFields │   │    tag-CRC merge, dedup  │
│ (retail       │   │ 13 DefFlags  =  THE TYPED IR (derives)   │   │    types, generated)  │   │    by (tag, bytes)       │
│  membership)  │   │ each type: parse / serialize / byte_size │   │ via Applier over      │   │  • NamesBuilder (shared) │
└──────────────┘   │ DefBinary / Chunk / EntryRecord / Names   │◀──│ FieldRef reflection + │   │  • 16 KB zlib chunks     │
                    │ semantic.rs: SemVal differ               │   │ LowerEnv link seam    │   └─────────────────────────┘
                    └─────────────────────────────────────────┘   └──────────────────────┘

 VERIFICATION:  golden.rs (FNV byte-determinism — THE regression gate)
              · verify.rs semantic ledger vs retail (THE divergence oracle, §5) · in-game load test
```

**Stage 0 — inputs.** Text corpus (`.def` = concrete `#definition`s, `.tpl` = abstract
`#definition_template`s; both must load so `specialises` chains resolve), C-like header
files (`#define`/`enum` constants), and the static manifest (per-binary membership +
NULLDEF lists, extracted once from retail).

**Stage 1 — text parsing** (`defs::def::text`). Recursive-descent parser → a small
AST. A definition body is a list of three statement forms:

```text
Health 100;                            → Statement::Field   (path, expr)
Controls.Add(CActionInputControl(…));  → Statement::MethodCall (object path, call)
<CTavernGameDef> … <\CTavernGameDef>   → Statement::TaggedBlock (tag, body)
```

Paths support members/indices (`States[2].ZoomX`); expressions are `Number(String) |
Bool | String | Symbol | Constructor | BitOr | Add`. Header files evaluate into a flat
`SymbolTable` (`HashMap<String,i64>`, `#ifdef _WINDOWS` honored, first-insert wins).

> **This layer is strict and token-based** (§11), covering both grammars
> (definitions + headers) on one lexer and — as of the R17 unification — one shared
> `Cursor` and one `TextParseErrorKind`. `Integer`/`Float` are merged into raw
> `Number(String)`, interpreted per-field at evaluation.

**Stage 2 — specialization flattening** (`lower::flatten_specialization`). The
`specialises` chain concatenates most-distant-ancestor-first into one statement list.
With downstream last-wins semantics this reproduces the game compiler's
copy-parent-then-apply. Same-tag tagged blocks **merge** (parent first), never replace.

**Stage 3 — lowering** (`def-compiler`). `lower_def` dispatches on def type name.
All def types (game/frontend/script) live in one unified table (§4.2); ~35 bespoke arms
handle C++-specific logic (§4.5); the remaining ~210 flow through
`lower_generic::<DefBody>`. All reference resolution goes through the two-method
`LowerEnv`, keeping lowering independent of index assignment.

**Stage 4 — assembly** (`defc::build`). Per binary: allocate our own
global-index space (NULLDEF region first, then named entries in first-seen corpus order,
filtered by manifest), lower every body, generate sub-def tables + deduplicated anonymous
sub-def entries (game.bin only), intern all strings into one shared `NamesBuilder`, pack
entries into ≤16 KB zlib chunks, serialize.

**Stage 5 — serialization** (`defs::def::binary` + `wire`). Every typed struct
serializes itself; the container layer adds headers, name-ref tables, the chunk index,
and the compression envelope (§4.7).

---

## 3. Central design fact: typed structs are the IR

**The typed def structs are the compiler's IR**; the pipeline is `text AST → typed
structs → bytes`. Each `#[derive(DefStruct)]` struct is simultaneously (1) the binary
layout spec, (2) the parse target when reading retail bins, (3) the lowering target when
compiling text, (4) the reflection subject for generic lowering and the differ. **Layout
is declared exactly once**, so parse/serialize/size can never disagree, and the golden
test pins the whole chain.

Proportions: ~4.5k lines of logic (engine) over ~22k lines of declarations (schema). Most
of the repo is schema, not machinery — the right shape for a format compiler, and the
reason most of the backlog (§9) is about moving *more* knowledge from machinery into
schema.

**Rejected alternatives** (don't relitigate): a mid-level "resolved statement" IR between
text AST and typed structs (adds a layer for the ~210 types that don't need one, changes
nothing for the ~35 bespoke arms whose logic is irreducible retail-compiler knowledge); a
dynamic/schema-interpreted def model (loses compile-time layout checking, makes every
bespoke arm stringly-typed).

---

## 4. Layer guide

### 4.1 Text layer (`defs::def::text`)

| File | Contents |
|---|---|
| `lexer.rs` | `lex` / `Lexer` → `Vec<Token>` (flat `TokenKind`, spanned); unified tokenizer for both grammars (§11) |
| `def_text.rs` | `DefParser` → `DefFile { definitions, by_name, headers }`; the AST types |
| `header.rs` | `HeaderParser` → `#define` / `enum` / `namespace` / `#ifdef` items |
| `symbols.rs` | `SymbolTable::evaluate` — header items → flat `name → i64` map |
| `base.rs` | Shared span infrastructure (`Span`, `Spanned<T>`, `LineIndex`, `ParseError<T>`); the old char-cursor machinery is gone (§11) |

> There is **no `text/manifest.rs`** and never was in this repo — the curated
> `SHARED_HEADERS`/`PC_HEADERS`/`XBOX_HEADERS` list this table used to name is from the
> pre-extraction OpenAlbion design. Header discovery is a recursive scan with explicit
> variant resolution; see **§4.1.1**.

Load-bearing properties:

- **AST nodes carry source spans** (`Spanned<Statement>`, `Spanned<Expr>`,
  `PathSegment::Index(Spanned<Expr>)`, `Spanned<Definition>`). `Spanned<T>: PartialEq`
  ignores the span. Spans are byte ranges (`&source[start..end]` reproduces the text;
  corpus files are CRLF, so `\r` is trivia).
- **Statement order is preserved** through parsing and flattening — load-bearing, because
  method-call DSLs (`Animation.StartGroup` … `Add` … `EndGroup`) and `Field.clear()` have
  positional semantics.
- **`SymbolTable` duplicates: the last definition wins**, and a duplicate is *never* fatal.
  `insert` returns the value it replaced; `evaluate`/`evaluate_items` return a
  `Vec<Redefinition>` that `build.rs` reports as warnings. Matches the C preprocessor and
  the retail tooling's `m_SymbolMap[name] = value`. Golden does **not** constrain this
  (the corpus has zero duplicate symbols), so the old "keeps the first value — validated
  by golden" note was wrong on both counts (§4.1.1).
- `object.rs` (data-format's `OBJECT → mesh` resolver) **reimplements the `specialises`
  walk** in stringly form — a drift risk (§9).

#### 4.1.1 Header discovery — variant sets

`Data/Defs` does **not** ship one flat set of headers. It ships *complete variants of the
same set*, along two axes:

| Axis | Variants | Resolution |
|---|---|---|
| Build | `RetailHeaders/` vs `DevHeaders/` | `HEADER_SET_ROOTS` in `build.rs`, most-preferred first; only the winner is scanned |
| Platform | `pc/` vs `xbox/` within a set | `IGNORED_PLATFORM_DIRS` skips `xbox` |

`RetailHeaders/` is exactly `DevHeaders/` minus `xbox/` — the same ~15 logical files
(`meshdata.h`, `text.h`, `pc/textures.h`, …). Reading both unions two copies of every
symbol into one namespace. **`RetailHeaders` wins**: it is the richer set (the `DevHeaders`
lipsync headers are empty stubs holding 0 symbols against 20,505) and it is the set the
modding tools write to. The two agree on every shared name/value pair, so the choice is
byte-neutral for a stock corpus — verified: `DevHeaders`-only, `RetailHeaders`-only, and
both-scanned all produce identical `game/frontend/script/names.bin`. It decides only
*which set a mod's edits are read from*. The Anniversary debug corpus ships only
`DevHeaders/`, so selection is a no-op there and golden is unaffected.

The ~47 `.h` at the corpus root are shared and always read. When a set is skipped the
build warns, naming it — a modder who patched the losing set otherwise sees their symbols
silently vanish.

> **The bug this exists to prevent** (2026-07, `DTB Hei Mask` / EgoCore): both sets were
> scanned; `DevHeaders/*` sorted first and claimed every symbol; each `RetailHeaders/*`
> file then hit a duplicate on its *first* enum variant, and — because a duplicate aborted
> the rest of the file — was discarded whole. ~44,900 symbol definitions dropped at
> warning severity. The mod had patched `RetailHeaders/`, so its five new symbols never
> reached the table and surfaced as one `unknown symbol` error 5,279 lines away in
> `objects_clothing.def`. Two independent defects: fatal duplicates, and unioned variant
> sets. Both are fixed; regression tests in `build.rs::tests` and `symbols.rs::tests`.

### 4.2 The schema layer — proc-macro derives (`defs-derive`)

The typed def model is declared with **five proc-macro derives**. The types are plain
Rust (rustfmt/rust-analyzer/spanned errors) with wire facts in `#[def(...)]` /
`#[flags(...)]` attributes.

| Derive | Instances | Generates |
|---|---|---|
| `DefStruct` | ~273 | `Default`/`DefDefault` + control-level `parse`/`serialize`/`byte_size` + `visit_fields` + `Wire` + `StructSlot` (`visit_named=true`) + `AsField` |
| `WireStruct` | 86 | compound value (members, **no** control ids): `Wire` + `DefDefault` + `StructSlot` (`visit_named=false`) + `AsField` |
| `DefVariant` | 4 | tagged union (`u32` tag + case fields): `Wire` + `VariantSlot` + `AsField` |
| `DefEnum` | 62 | closed i32 enum with C++ symbol names; out-of-table parse is an **error** |
| `DefFlags` | 13 | bit-set newtype (for "enums" the game ORs together) |

```rust
#[derive(Debug, Clone, PartialEq, DefStruct)]
pub struct CreatureDef {
    #[def("PhonemeAnim")]    pub phoneme_anim: VecMap<String, i32>,
    #[def("AllowedInProtectedTowns", default = true)] pub allowed_in_protected_towns: bool,
    //   wire name (crc32 id)          ctor default        rust field        wire type
}
```

`#[def("WireName")]` = the field's def-script name (its `crc32` id); `default = <expr>`
overrides `DefDefault` for the never-parsed NULLDEF body. `tag = "sibling_field"` marks a
**polymorphic field** whose in-memory variant is chosen by an earlier-declared sibling's
value (see `UiDef.mesh_index`/`MeshRef`, §4.8): the derive parses fields into locals in
declaration order, so a tagged field routes through `TaggedWire::parse_tagged(cur, tag)`
with the sibling in scope. The namespace is deliberately open so R4/R5 land as *named* args
(`container = …`, ctor-arg maps) without breaking these forms.

**Wire encodings** (all byte-verified against every entry of all three retail bins):
`f32`/`i32`/`u32`/`u16`/`u8` LE; `bool` 1 byte; `String` = NUL-term UTF-8; `WStr` =
NUL-term UTF-16LE; `PString` = u32 len + bytes; `DefString` = i32 names.bin offset
(default −1); `DefIndex` = i32 global index (default 0); `Vec<T>` = u32 count + elems;
`[T;N]` = N elems no count; `BTreeMap` = u32 count + key-ordered pairs (`std::map`);
`VecMap` = same shape, stored order preserved (`CVectorMap`); `DefEnum` = 4 bytes must be
in table; `DefVariant` = u32 tag + case fields.

A `DefStruct` body is a sequence of **field controls**: `u32 crc32(wire name)` + wire
value, in declaration order. `crc32` ids are **validated, not dispatched**. Runtime
helpers (`parse_field`/`serialize_field`/`field_size`, scalar/container `Wire` impls,
`DefDefault`) live in `wire.rs`; the derives only generate per-type glue.

The `DefBody` enum + its dispatch (`parse`/`serialize`/`byte_size`/`visit_active`/
`def_default_for_name` + `impl VisitFields`) is generated by a local `def_body!` macro in
`def_binary.rs` from an inlined table of ~247 `Variant(Type) => ["WireName", …]` rows
covering every def type from all three binaries. One hand-written `Unknown` fallback
completes the enum. **Proc macros can't enumerate a crate's types, so this central list
is irreducible** — it is `DefBody`-enum duty only.

### 4.3 Reflection layer (`visit.rs`)

`FieldRef<'a>` is a typed mutable handle to one field (one variant per wire kind, plus
`Vec`/`Map`/`Struct`/`Variant` slot-trait objects and a `Complex` escape hatch).
`visit_fields` pushes each field with its **wire name**; compounds/variants expose
positional `StructSlot`/`VariantSlot`; maps expose `MapSlot` with an entry-builder.
Consumers: generic lowering (mutating walk) + semantic differ (reading walk). Known limits
(all from the `&mut`-only design): `MapSlot::for_each_pair` clones keys/values to read them;
three overlapping surfaces. `FieldRef::Array` + `ArraySlot` (R9, landed) cover fixed arrays,
so the differ's former `OpaqueOnly` blind spot is closed.

### 4.4 Lowering engine (`def-compiler`)

Three primitives in `reader.rs`:

- **`Evaluator`** — single source of truth for evaluating one `Expr` against the symbol
  table. Handles `NULL` (→0), `TRUE/FALSE/BTRUE/BFALSE`, `|` and `+` folds. Deliberate
  asymmetries (e.g. `f32` doesn't accept `NULL`) reflect the corpus — extend only with
  corpus evidence. Number interpretation lives here: raw `Number(String)` is parsed per
  target type (float-shaped-in-int truncates; `f32` strips a trailing `f`) — §11.
- **`Args`** — positional reader over ctor/method-call argument lists.
- **`DefReader`** — scans statements by name at a path depth; on duplicates **last wins**
  (specialization concatenates parent+child, so last-wins realizes child-overrides-parent).
  Combinators: `group`, `indexed_sparse`, `keyed`, `calls`, `any_*`.

**Pull model.** The obvious "iterate statements, look up field by name" is blocked by the
borrow checker (can't hold a `name → FieldRef` map — aliasing `&mut`). So `lower_generic`
clones the NULLDEF base and the `Applier` (a `FieldVisitor`) walks fields via reflection,
each field *pulling* its matching statements. Order-sensitive semantics that don't survive
inversion are handled by a pre-pass (`strip_superseded_by_clear`) or by interception
before the generic walk (method-call DSLs). Unconsumed statements are silently dropped
(matches C++ `Transfer` skipping unknown field names — needed for cross-type
specialization).

**`LowerEnv`** is the whole link-time interface — `def_index(name)` + `def_string_offset(str)`.
`ScratchEnv` implements it over the builder's own index allocation + shared `NamesBuilder`.

### 4.5 Bespoke lowering catalog

Everything the generic walk can't do lives in ~35 arms in `lower_def` (lower.rs); the
decomp is the authority.

**A. Method-call DSL interception** (statement-order replay of a C++ builder API):
`CAnimatingObjectDef`/`CAppearanceDef` (`Animation.Add/StartGroup/…`, key=crc(name),
stable-sorted); `CCreatureDef` (`Expressions.Add`, `WoundMorphs.*`, 40-byte packets);
`CEntitySoundDef` (`SoundMap.*`, crc-keyed, sorted after each add); `CFlammableDef`
(`std::map<CDefString,…>` ordered by TablePos not strcmp); `CHeroMorphDef`;
`OPINION_DEED_EFFECTS` (seconds→frames ×15, **x87 single-rounding** via f64 intermediates);
`OPINION_PERSONALITY` (5×36-byte blob, args 6/7 swap into slots 7/6);
`OPINION_REACTION_MANAGER` (ctor families precompute inverse radii / per-frame rates,
hurried-set at idx+79); `CSpecialEffectsDef` (crc-keyed, dup insert = no-op **first** wins,
then sort); `CDegradableDef`; `CReplaceableMeshDef`; `CAppearanceModifierDef` (24-byte
packet, args reordered); THING family ×10 (`Components.Add/Remove`, one `lower_thing!`
macro, DriverType from the static 13-entry registry, universal `trailing_u32=28_011_726`).

**B. Container-semantics fixups** (post-walk): `CCombatAbilityBlock*`
(`ValidBlockWeaponTypes` is a `std::set` → sort+dedup); `PLAYER_GUI` (`VecMap` sorted by
crc32(key)); `OPINION_REACTION_MANAGER` (eight `VecMap` fields sorted by key). → schema-ize
as R4.

**C. Quirks:** `CAMERA_MANAGER` (consume `CameraList.clear()`); `CWeaponDef`
(`WeaponTrails` stored swapped); `CWillResponseDef` (strip Anniversary-only
`ForceLightningable`); `OPINION_SOURCE` (derive 79 bools from flag defaults+maps);
`CLookDef.CombatMaxTurnSpeed` (naive default propagation regresses ~503 entries — 1-entry
divergence accepted, NOT implemented).

**D. Compound ctor arg permutations** (in `apply_struct_from_expr`, → R5):
`BlendedParticleEffectSet`, `ObjectAugmentationParticleSet`, `ExplosionRing` (0/1 swap),
`ParticleAttachmentInfo` (0/1 swap), `AttackHistoryCombo` (multiplier-first vs vec-first).

**E. Hand-written frontend lowerings** (`lower.rs`): `UI`, `CONTROL_SCHEME`
(`CActionInputControl` slot dispatch by controller type), `UI_MISC_THINGS_DEF`,
`FRONT_END`. The other frontend types (`ENGINE`, `ENVIRONMENT`, `ENVIRONMENT_THEME_DAY`,
`ENGINE_VIDEO_OPTIONS`, `CONFIG_OPTIONS_DEFAULTS_DEF`, `UI_ICONS_DEF`) flow through generic.

### 4.6 Assembly layer (`defc::build.rs`)

One pipeline, three instantiations (game/frontend/script differ only in file collection,
manifest slices, and the game-only sub-def region):

1. **Index allocation** (`collect_named`): global index = position. NULLDEF region first
   (`*_NULLDEF_ENTRIES` order), then one index per distinct named def in first-seen corpus
   order, filtered by `*_NAMED`. File order = sorted path list; ~800 duplicate instance
   names exist and **the whole later body replaces the earlier one** (last-processed-file
   wins). Duplicate names currently produce no diagnostic (§12.6).
   `UI_MISC_THINGS_DEF_INSTANCE` was the notable case — the def is declared in 4 files and
   our replace keeps only the last file's body, which matches retail's `CompileDefinition`
   behavior (last file fully wins, NOT a merge).
2. **NULLDEF bodies**: `lower_def(class, empty)` = the type's `def_default()`. Preamble
   `(false,false,0)`.
3. **Named bodies**: flatten → lower. Preamble `(true,false,1)` — the `1` is
   `AreDefaultValsApplied`, load-bearing (0 ⇒ save-load crash). **No fallback**: a def that
   fails to lower is not emitted (the old NULLDEF-fallback was deliberately removed — §11).
4. **Sub-defs (game.bin only)**: merge same-tag tagged blocks across the specialization
   chain (CRC of tag, parent-first concat), lower each, serialize, dedup anonymous entries
   by **(class-tag, body bytes)** — byte-only dedup type-confuses combat sub-defs and
   crashes save-load. Anonymous `file_name_offset = 0xFFFFFFFF`.
5. **names.bin**: one shared `NamesBuilder` across all three; header words at +8/+12 are
   content-derived (StringCount, StreamLength = size−16).
6. **Chunking**: greedy split at 16 KB decompressed, zlib level 1 (`78 01`).

### 4.7 Binary container layer (`defs::def::binary`)

```text
DefBinary   := header(13B) · NameRef[n](12B) · ChunkIndexHeader(8B)
             · ChunkIndexEntry[chunks](8B) · sentinel ChunkIndexEntry (REQUIRED —
               chunk offsets are relative to the region after it) · zlib chunks
EntryRecord := preamble(3B: is_real, is_template, AreDefaultValsApplied)
             · [sub-def table: u16 count · 12B records]  — iff type derives the sub-def bases
             · body: field controls (u32 crc32(name) · wire value)…
names.bin   := 20B header (off8=StringCount, off12=StreamLength) · (u32 crc · NUL utf8)…
```

- **Sub-def table presence is a per-wire-name property** (`def_name_has_subdef_table`,
  generated from `sub_def_names!` in `def_binary.rs` — 106 wire names deriving
  `CSubDefClassBase`/`CParentDefClassBase`, incl. script.bin's
  `CScriptDef`/`CCutsceneDef`/`CRegionScriptDef`). Present ⇒ the u16 count is always
  serialized, even when 0.
- **Parse is total with typed fallback**: unknown type → `DefBody::Unknown{raw}`; a known
  type whose bytes don't match propagates an error and the entry keeps raw bytes intact.
- **Two error styles coexist**: the wire layer uses composable context wrapping
  (`ParseWireError::Member{name}/Item{index}` — the good pattern); the container layer uses
  one-enum-per-function. Unification candidate (§9 Track C).

### 4.8 Reference semantics (cross-cutting)

- **External refs are by NAME; in-body refs are global indices.** The engine looks up
  Thing→def by instantiation name, which is why own-order index assignment is safe.
- **A field's *type* declares whether it is a reference** (RT phase, landed). The ≈90
  once-mistyped reference fields are now `DefIndex` (wire = `i32` LE, resolved via
  `resolve_ref_i32`); plain `i32`/`u32` fields evaluate strictly, so a typo in a numeric
  field errors instead of silently becoming a def index. Corpus-measured: 94% of integer
  fields are purely numeric, ~5.5% purely references, ~0 genuinely both. UI reference
  containers/scalars (`children`/`sprites`/`down_arrow`/…) are `DefIndex`/`Vec<DefIndex>` too.
- Some `DefIndex`-typed fields hold **hashed text ids** (`CShopDef.Name`,
  `CTavernGameDef.Banter…`) — they look out-of-range but are load-safe. So cross-index
  verification can't treat every `DefIndex` as a reference (§5).
- **Genuinely polymorphic refs** — one wire field whose meaning depends on a *sibling*
  field — are modeled as an enum (structs are the IR, not the wire — the variant serializes
  to the same scalar). Canonical case: `UiDef.mesh_index`/`animation_index` = `MeshRef {
  Bank(u32), Def(DefIndex) }`, disambiguated by `Type` (`UI_TYPE_MESH`/`MOUSE_POINTER` →
  a graphics.big **bank id** compared raw; `UI_TYPE_LIST`/`SCROLLING_VIEWPORT` → a **UI-def
  ref** resolved by name). The `#[def(tag = "type_")]` attribute (§4.2) drives the
  Type-aware parse; `AsField` dispatches `Bank→U32`/`Def→DefIndex` so the differ needs no
  new arm. Confirmed against graphics.big via OpenAlbion's `fable-data::big`
  (`asset_by_id`: `MeshIndex` 8033 → `MBANK_ALLMESHES/MESH_HERO_IRON_BATTLEAXE`).

---

## 5. Verification & the bug-fixing loop

Three verification layers, each proving something different:

| Layer | Proves | Does NOT prove |
|---|---|---|
| **`tests/golden.rs`** (FNV hash of all 4 outputs vs `golden_manifest.txt`; needs `OA_TEXT_DIR`; re-bless `OA_BLESS=1`) | Build is deterministic and byte-identical to the *blessed* build — **the regression gate for every refactor** | That the blessed build is *correct vs retail* (it compares to our own last-blessed output, which can bake in bugs) |
| **the `verify` example** (`def-compiler/examples/verify.rs`; run `--example verify -- $OUT $REF`) — decodes both sides to reference-resolved `SemVal`, classifies each named entry `Reproduced`/`AcceptSort`/`OpaqueOnly`/`Bug`/`Missing` | **The divergence-from-retail oracle.** This is where real bugs surface | Anything about *modified* defs (values retail never shipped) — only in-game does |
| **In-game load** | The only proof that a *modified* or *fixed* def actually works | — |

`SemVal` (`semantic.rs`) decodes bodies through reflection, resolving `DefIndex→name` and
`DefString→string` per side, so two independent index spaces compare by *meaning*.
`DiffPolicy::unordered()` compares containers as multisets to separate MSVC sort-tie-break
noise from real diffs.

**Anonymous sub-defs are now covered too** (`verify_subdefs`). Named entries reference their
compiler-generated sub-defs by `(tag CRC → global index)` in the sub-def table, so sub-defs
are matched across the two index spaces by **(parent instance name, tag)** — the one identity
that survives — and each pair is classified with the *same* `Reproduced`/`AcceptSort`/
`OpaqueOnly`/`AcceptArtifact`/`Bug` logic as named entries. This surfaced ~5,400 previously
invisible sub-def divergences; a dedicated `count-mismatch` counter proved the anonymous-entry
**duplication** difference (our dedup pool has fewer distinct entries than retail) is pure
physical sharing — **every parent references the same *number* of sub-defs as retail** (0
count-mismatches) — not a data problem. The ledger renders a `SUB-DEFS (by parent+tag)` section.

> **⚠️ Correction to a former belief.** This doc used to call the ledger "informational
> only — `Bug`s are mostly noise." **That was wrong and hid real bugs.** In 2026-07 the
> `Bug` bucket was found to contain two large systematic *default-value* bugs (UI `Font`
> defaulting to null instead of `ENG_ARIAL_16`; THING `PersistenceFlags` defaulting to 0
> instead of `EPF_STATIC`) affecting ~4,900 entries — masked as "index noise." **Treat the
> ledger as the primary bug-finding oracle**, not noise. Golden cannot see these because it
> only compares to our own (possibly buggy) baseline.

### Sources of authority (which input to trust when they disagree)

Ranked. Higher wins. This ordering was learned the hard way — see the `TemplePrayerFactorHighest`
episode below.

1. **The retail binary itself** (`$REF`), read back through our own parser. Because parsing
   is **positional** (each field control is `crc32(name)` + value, read in *declaration
   order* and crc-validated — §4.2), **loading a retail binary under our schema is a
   structural oracle**: a `WrongId { expected, found }` error pinpoints a field
   count/order/name mismatch at the exact byte. `probe_field --entry NULLDEF_<TYPE> --vs $REF`
   dumps a decoded entry field-by-field.
2. **Retail NULLDEF entries = the authoritative source for field DEFAULTS.** A NULLDEF body
   is the def's default-constructed state serialized with no text applied. So retail
   `NULLDEF_UI.Font = 224` *is* the correct default. Read them straight off `$REF`.
3. **The decomp `Transfer<T>` functions** (`~/git/fable-decomp/out/source/**/<class>.cpp`) are
   **authoritative for wire types and field order**. `Transfer<long>` ⇒ `i32`,
   `Transfer<float>` ⇒ `f32`, `Transfer<CDefString>` (or a `.TablePos` member) ⇒ `DefString`.
   The order of `Transfer<…>` calls in the function *is* the binary field order.
4. **Text source defs** (`$TEXT`) — useful for *what a field means* and for finding which
   defs set which fields, **but NOT authoritative for layout**: fields may be ordered
   differently than the binary, and some text fields are unused/dropped by the compiler.
   Note: a field the text writes as a **string** can legitimately carry `0` = *null string*
   (a valid `DefString`), so "string-typed" and "sometimes 0" are compatible — don't
   conclude such a field is numeric.

**NOT authoritative — do not trust for types:**
- **`defs-spec.json`** — an early automated extraction that *produced* the current schema
  and is the *source* of several mistypings (it wrongly said `Font`=long, `PersistenceFlags`
  =defindex). When a field looks wrong, go to the `Transfer<T>` function, not the spec.
- **The C++ copy-operator / `TransferIn` functions** — noisy and misleading. The
  `TemplePrayerFactorHighest` field is declared **twice** on purpose because retail's
  `CScriptDef::Transfer` emits two consecutive controls with that crc; the copy-operator
  showed only one, and "fixing" the duplicate broke positional parse of retail's script.bin.
  **Trust the binary (via positional parse), not the copy-op.**

### The bug-fixing loop (the "onion")

Fixing divergences is iterative: the ledger reports one representative field per entry, so
fixing the first-diverging field **reveals the next one underneath**. Expect to peel.

1. **Find** — run the ledger (`--example verify -- $OUT $REF`); read the `BUG diff paths` (a
   `type.field → count` histogram) and the samples.
2. **Classify** each field: *value divergence* (real bug) vs *index-space noise* (a
   reference field showing different raw indices that resolve to the same def name — inherent
   when comparing two independent index spaces; only fixable by re-typing to `DefIndex` so
   the differ resolves it, and only when the target is a *named* def, not an anonymous
   sub-def). `probe_field --vs $REF` compares by resolved meaning to tell these apart.
3. **Diagnose** — establish the correct type/default from the authority ranking above. For a
   whole struct's defaults at once, `probe_field --entry NULLDEF_<TYPE> --vs $REF`.
4. **Fix** in the schema (`#[def(..., default = …)]` for defaults; change the field type for
   mistypes; bespoke-lower seeding in `lower.rs` when a default needs interning, e.g. a
   `DefString` default — see `lower_ui`'s `ENG_ARIAL_16` seed).
5. **Verify** — re-run the ledger; the fixed field should drop to 0 divergences; confirm no
   *new* regressions (Bug count monotonically down, Reproduced up).

### Byte-identical vs byte-changing fixes

- A **type fix that doesn't change encoding** (`DefIndex`↔`i32`↔`DefFlags` — all 4-byte LE;
  numeric/symbol values lower identically) is **byte-identical**: golden stays green, no
  re-bless. These improve type-safety and de-noise the ledger for free. Do them freely.
- A **default or structural fix changes bytes**: golden **fails by design**. Workflow:
  verify against `$REF` (0 divergence on the fixed field), **deploy + in-game test**
  (§10 commands; rollback from `$REF`), then re-bless with `OA_BLESS=1`. Do **not** re-bless
  a byte-changing fix without the in-game load test.

**No CI exists** (a `.github/` workflow was planned but never landed / was dropped in the
monorepo move) — the gate is discipline. Re-adding it is a quick win (§9).

---

## 6. The manifest

`defc/src/manifest.rs` (~10.6k generated lines; regenerate with the
`dump_manifest` example) is the **only retail-derived input**. It encodes exactly two
things text can't provide: **membership** (which text defs retail shipped per binary) and
**NULLDEF lists** (which classes get NULLDEF entries, order, and the duplication quirk —
`CONTROL_SCHEME` appears twice in frontend.bin). Deriving membership from text would change
the contract from *retail-equivalent* to *superset*, raising the in-game verification
burden for no gain. **Keep it** — small, static, regenerable, honest.

---

## 7. Settled decisions

Keep as-is (don't relitigate): the typed-struct IR and single-declaration layout principle
(§3); proc-macro derives as the declaration mechanism (serde rejected — no in-place
mutation; spec-codegen rejected — the Rust declarations have absorbed byte-verified
corrections *against* the spec); the `DefReader` pull model (a legitimate answer to
`&mut`-aliasing, verified against 12,604 entries — don't rewrite for elegance); the
`LowerEnv` two-method seam; the manifest's existence (§6); the 16 KB chunk envelope (output
is ~1.17 MB larger than retail but content is byte-identical — the size gap is the TARGET,
not a correctness gap).

Facet reflection was evaluated as the long-term home for `visit.rs` (its `Shape`/`Peek`/
`Poke` map onto the hand-rolled reflection) but deferred — it's 0.1.x/experimental; revisit
via the R12 spike only.

---

## 8. Divergence closure — completed

The divergence-onion phase is complete. The semantic ledger (§5) now surfaces only verified
genuine artifacts.  **Summary of remaining divergences** (2026-07-25):

### Named ledger

| Binary | Reproduced | AcceptSort | AcceptArtifact | Bug | Missing |
|---|---|---|---|---|---|
| game | 8,622 | 2 | 0 | 6 | 9 |
| frontend | 800 | 0 | 0 | 1 | 0 |
| script | 608 | 0 | 0 | 0 | 0 |

The 7 named `Bug` entries are all accepted artifacts (do not chase):
- **BUILDING.Graphic.anim_step** (2 entries) — cross-type specialisation: BUILDINGs that
  specialise OBJECT templates; retail byte-copies mismatched layouts.
- **OBJECT.RotationTime** (1) — same; OBJECT specialising BUILDING template.
- **INVENTORY_TYPE.TradeShadedCircleTLPos.x** (1) — version drift: text says 220, retail shipped 200.
- **SOUND_SETUP.MusicSetEntries[...].loop_count** (1) — uninitialized memory (~322M in retail).
- **OPINION_REACTION_MANAGER** (1) — 1-ULP x87 rounding artifact in `neg_inv_y_radius`.
- **UI.States[...].graphic_index** (frontend, 1) — version drift / frontend_test vs pc_frontend.

### Sub-def ledger (game.bin)

| Class | Count | Tags |
|---|---|---|
| Reproduced | 32,318 | |
| AcceptSort | 487 | CAppearanceDef (486) + CHeroSuitDef (1) — MSVC unstable `std::sort` tie-breaks |
| AcceptArtifact | 719 | See breakdown below |
| Bug | 1 | CLookDef.CombatMaxTurnSpeed — accepted |

### AcceptArtifact breakdown (all verified genuine)

All 719 entries are **extra serialized bytes the engine never reads meaningfully**:

| Count | Tag | Path | Mechanism |
|---|---|---|---|
| 517 | CCreatureDef | WoundMorphs.trailing_u32 | CVectorMap writes count, then entries, then a second stale-register WriteSLONG. TransferBinaryIn **skips** these 4 bytes (discards on load). All entries have the identical value per type (WoundMorphs=0x01AB6CEE, Expressions=0x01AB6D32). |
| 169 | CObjectAugmentationsDef | AugmentationParticles[...].particle_effect_to_blend_to | NULL key (AUGMENTATION_NULL) ctor has count=0, leaves particle_effect fields **uninitialized**. Engine never references NULL-key particles. |
| 28 | CWeaponDef | FlourishParticles.particle_effect_to_blend_to | 28 trap/arena objects that have CWeaponDef but don't set FlourishParticles. Default ctor leaves fields uninitialized. |
| 4 | CHeroMorphDef | TextureMorphs.trailing_u32 | Same stale-register CVectorMap mechanism as CCreatureDef. |
| 1 | CDegradableDef | Degradations[...].skip[...] | 25-byte struct padded to 28 bytes; WriteSLONG writes alignment padding. WASP_NEST_01 element caught a 0x01AB6CE3 stale value; all other entries match at [0,0,0,0]. |

**The AcceptArtifact bucket was 1,384 before the 2026-07 particle+weapon round.** The reduction came from:
- CWeaponDef 693 → 28: 665 FlouishParticles entries had symbols auto-interned instead of resolved as particles.h enum values. Fixed.
- 14 CWeaponDef entries became Bug then Reproduced: WeaponTrails duplicate-key dedup. Fixed.

### Bug history

Full documentation of every divergence fixed is in `git log`. Key rounds:
- **2026-06:** Font, PersistenceFlags, UI SetDefaults, region-script counts (~4,900 entries)
- **2026-07 hero:** CHeroExperienceDef, CAppearanceDef, CHeroMorphDef, CDegradableDef, R9 differ totality (16 → 1 sub-def Bug)
- **2026-07 particle+weapon:** FlouishParticles/AugmentationParticles symbol resolution (~665 entries), WeaponTrails dedup (14 entries). Acceptance bucket 1,384 → 719.

---

## 9. Current roadmap

The divergence closure is done. The compiler produces retail-equivalent output verified at
zero genuine differences aside from the accepted artifacts (§8). The focus shifts to
**polish, safety, and usability** — making the compiler a tool modders can rely on.

### Track A — Compiler diagnostics (§12)

The divergence hunt proved the compiler is **correct** on valid input. But it is **silent
on invalid input**: misspelled field names, wrong symbols, out-of-range values, and
unconsumed statements all produce correct-looking binaries with no diagnostic. The biggest
ticket items:

1. **`drop_remaining` warning** — unconsumed statements in generic lowering are silently
   dropped (P0, affects ~210 def types). A 50-line change.
2. **Bespoke-arm error propagation** — `.ok()` / `.unwrap_or(0)` chains in ~19 hand-written
   arms swallow invalid symbols, component names, animation keys, etc.
3. **Sub-def failure strictness** — sub-def lowering failures are currently emitted as
   *warnings*, not errors; the build succeeds with missing sub-defs.
4. **Integer narrowing** — `*slot = v as u8` truncates without range check.
5. **Duplicate def name warning** — ~800 duplicate instance names silently overwrite.

### Track B — Enum typing (§13)

~34 fields across ~15 files are typed as plain `i32`/`u32` when existing enum types could
be used. All are **byte-identical** (DefEnum/DefFlags serializes as i32 LE). Priority:

1. **UiDef** (21 fields): `type_`, `expansion_type`, `mesh_type`, `alignement`,
   `sprite2_d_flag`, 16× `action*` fields
2. **UiStateDef** (1): `state_change_type`
3. **InventoryItemDef** (1): `tutorial_category`
4. **Thing* family** (10): `persistence_flags` — needs new `PersistenceFlags` DefFlags
   extracted from decomp

### Track C — Quality & architecture

- **Re-add CI** — `.github/workflows/ci.yml` with `build`, `clippy -D warnings`, `test`
- **Kill dead lowering functions** — `lower_engine`, `lower_config_options_defaults`,
  `lower_engine_video_options`, `lower_ui_icons` (~100 lines in `lower.rs:278-421`) are
  never dispatched; all four types fall through to the generic `_ =>` arm in `lower_def`.
  Either wire them into the dispatch table or delete.
- **Add unit tests for lowering** — `lower.rs` (3,382 lines) has **zero** unit tests. The
  bespoke arms in particular (ctor arg remapping, DSL replay, sort/dedup fixups) would
  benefit from targeted tests. The `defs` crate has extensive tests; the lowering layer
  relies entirely on the golden end-to-end gate.
- **Kill object.rs drift (R8)** — re-express over `flatten_specialization`
- **Assembly into library (R6)** — move pipeline into `def-compiler`; collapse per-binary duplication
- **Schema-ize bespoke lowering (R4/R5)** — `#[def(container = …)]`, declarative ctor-arg maps
- **Facet spike (R12, optional)** — port differ to facet as tracer bullet

---

## 10. Operational reference

### Commands

```bash
REF=~/Fable/data/CompiledDefs/backup-retail-verified
TEXT=~/doc/Fable_Anniversary-2013-02-25/Fable/Data

# From-scratch build (all four binaries)
cargo run -q -p defc -- $TEXT/Defs <out_dir>
# Semantic ledger vs retail (build first, then run the verify example on the output)
cargo run -q -p def-compiler --example verify -- <out_dir> $REF
# Full SemVal dump of one parent's sub-defs (both sides), for a divergence
cargo run -q -p def-compiler --example verify -- <out_dir> $REF --dump-subdef <Parent> [<Tag>]
# Golden byte-determinism gate (THE regression gate)
OA_TEXT_DIR=$TEXT/Defs cargo test -p defc --test golden
# ...re-bless after an INTENDED output change:
OA_BLESS=1 OA_TEXT_DIR=$TEXT/Defs cargo test -p defc --test golden
# Regenerate the manifest (only if retail reference changes)
cargo run -q -p def-compiler --example dump_manifest -- $REF packages/defc/src/manifest.rs

# Probe/compare a single field vs retail (the divergence-investigation workhorse — §5)
cargo run -q -p def-compiler --example probe_field -- <dir> <game|frontend|script> <FieldName> --vs $REF
# Dump one entry's fields and diff vs retail (the default-gap finder)
cargo run -q -p def-compiler --example probe_field -- <dir> game --entry NULLDEF_UI --vs $REF

# In-game test — deploy all four; ROLLBACK from $REF if it won't load
cp <out_dir>/{game,frontend,script,names}.bin ~/Fable/data/CompiledDefs/
cp $REF/{game,frontend,script,names}.bin ~/Fable/data/CompiledDefs/   # rollback
```

### Key files

| File | Role |
|---|---|
| `packages/defs-derive/src/lib.rs` | The five schema derives + `#[def]`/`#[flags]` attr parsing |
| `packages/defs/src/def/binary/def_binary.rs` | Container: `DefBinary`/`Chunk`/`EntryRecord`; generated `DefBody` + `sub_def_names!` (the canonical def table is inlined here) |
| `packages/defs/src/def/wire.rs` | Wire model + runtime helpers |
| `packages/defs/src/def/enums.rs` | `DefEnum`/`DefFlags` + all 75 enum tables |
| `packages/defs/src/def/visit.rs` | `FieldRef` + slot traits (reflection) |
| `packages/defs/src/def/semantic.rs` | `SemVal` reference-resolving differ |
| `packages/defs/src/def/text/` | `def_text.rs` (parser+AST), `header.rs`, `symbols.rs`, `base.rs` (§4.1, §11) |
| `packages/def-compiler/src/reader.rs` | `Evaluator`/`Args`/`DefReader` |
| `packages/def-compiler/src/lower.rs` | Hand frontend lowerings; `flatten_specialization`; `Applier`; ~35 bespoke arms; `lower_def`; `lower_generic` |
| `packages/defc/src/build.rs` | From-scratch assembly (all four outputs) |
| `packages/defc/tests/golden.rs` | **Byte-determinism gate** |
| `packages/def-compiler/examples/verify.rs` | The retail semantic ledger + SemVal differ + `--dump-subdef` — the divergence oracle (an example binary; `defc` no longer embeds it) |
| `packages/def-compiler/examples/probe_field.rs` | Field-level probe/diff vs retail (histogram, `--vs`, `--entry`) — §5 |

### Load-bearing gotchas

- **CRC32** = standard zlib (poly `0xEDB88320`, init 0, no final xor) = `CCharString::GetCRC`.
- **`EntryPreamble.unknown_0 = 1`** for ALL non-NULLDEF entries (`AreDefaultValsApplied`;
  `0` ⇒ save-load crash). NULLDEF preambles are `(false, false, 0)`.
- **Tagged blocks MERGE** across specialization (parent-first concat), never replace.
- **Anonymous entry `file_name_offset` = `0xFFFFFFFF`**; dedup keys on **(class-tag, bytes)**
  (byte-only type-confuses combat sub-defs → crash).
- **`names.bin` is ONE shared table** across all three; header off8/off12 content-derived.
- **External def refs are by NAME; in-body refs are global indices.**
- **`DefString::def_default()` = −1; `DefIndex::def_default()` = 0.**
- **Text-tag fields mis-typed as `DefIndex`** (`CShopDef.Name`, `CTavernGameDef.Banter`…)
  hold hashed text ids — "out of range" but load-safe.
- **The chunk-index sentinel entry is required**; chunks compress at zlib level 1.
- **CONTROL_SCHEME appears twice** in frontend.bin NULLDEFs — iterate `NULLDEF_ENTRIES`
  (non-deduped) for output, build bodies from `NULLDEF_CLASSES` (deduped).

### Decomp reference map

| What | Where |
|---|---|
| CRC32 | `bbblibrary/lib_crc.cpp` |
| Def / sub-def compile | `fablelib/defs/definition_manager.cpp`, `bbblibrary/lib_definition_manager.cpp` (`CompileSubDefinition`) |
| Def lookup | `lib_definition_manager.hpp` (`GetPDefFrom{GlobalIndex,InstantiationName}`) |
| names.bin format | `bbblibrary/lib_definition_string.cpp` (`CDefStringTable`) |
| Thing def ref = name | `fablelib/thing.cpp`; anon entry sentinel `thing_base_def.cpp` (TablePos = −1) |
| Animation / opinion / sound / controls | `animation_set.cpp`; `tc_opinion_of_hero.cpp` (ctor :4438), `opinion_reaction_manager.cpp`; `lib_sound_map.cpp`; `fablelib/defs/controls_def.cpp` |
| Thing components | `CThingComponentSet::Add` (driver types via `GetPTCInfo`) |
| Script def inheritance | `fablescripting/defs/{script_def.hpp:258,cutscene_def.hpp,regionscriptdef.hpp}` |
| **Wire types & field order (AUTHORITATIVE)** | the decomp `<Class>::Transfer` functions in `~/git/fable-decomp/out/source/**/*.cpp` — §5. Grep the class name; the `Transfer<T>(…, &this->Field)` calls give type + order. |
| Layout spec (NOT authoritative) | `fable-decomp/defs-spec.json` — an early extraction that *produced* several mistypings; superseded wherever it disagrees with retail/`Transfer<T>` (§5). |

---

## 11. Text layer — design notes & rationale

The text layer (§4.1) was rebuilt from an error-tolerant char parser into a
strict, token-based one: a unified lexer feeding a def parser and a header parser
that share it — one lexer, one `Cursor`, one `TextParseErrorKind` (the R17
unification landed, so the def and header parsers are a single grammar surface).
The rearchitecture is **complete and golden byte-identical**; this section keeps
only the load-bearing *decisions and findings* (the phase-by-phase history lives in
git).

### Why strict parsing is safe (the decisive corpus finding)

Instrumenting both tolerance paths of the old char parser over the full corpus
(174 non-deprecated files) showed its skip-to-newline recovery fired **0 times** —
the "corpus contains stray tokens" premise was a myth (the old
`stray_tokens_recovered` / `building_herocentre.def` justification was false; that
region is well-formed). The only genuine tolerance in use is the **optional `;`**
terminator, hit by exactly 2 field statements — a clean grammar rule, not
malformed-input recovery. So strict parsing costs **zero golden bytes**.

The bug this fixed: a missing `#end_definition` mid-file used to be **silently
swallowed** — the body loop's skip-recovery consumed the next `#definition` and
its whole body until it found *that* def's `#end_definition`, so the inner def
vanished and only a downstream "unknown parent" cascade appeared. Now it is a
precise, anchored error (regression test:
`missing_end_definition_does_not_swallow_next_def`).

### Two error tiers, one policy each (no configurable failure policies)

- **Parse errors: strict, one per file, fail-fast, no recovery/resync.** The first
  error aborts that file (its defs drop), is rendered, and the build moves on.
  `parse_def_file` returns `Result<DefFile, DefParseError>` — no diagnostics-vec in
  the parser. A file that fails to parse **must always be surfaced** (its defs
  silently vanish otherwise).
- **Compile diagnostics: collected, many per def** (§11 diagnostics, below). **No
  fallback default def** — a def with ≥1 compile error is not emitted and the build
  exits non-zero after reporting everything. The old NULLDEF-fallback was
  deliberately removed; it hid bugs.

### The lexer classifies and delimits; it never interprets

Numbers stay raw (`TokenKind::Number` = the source slice) and are interpreted
per-field during evaluation (`reader.rs::Evaluator`); strings keep their quotes and
are unquoted in the parser. `TokenKind` is flat, `Copy`, payload-free; `Token`
bundles `kind` + `span` + the raw `source` slice, so value tokens are read without
threading the input. Contextual words (`specialises`, `TRUE`/`FALSE`/`BTRUE`/
`BFALSE`, `NULL`) stay `Ident` — recognized by the parser/evaluator, not the lexer.
Greedy `<<`→`Shl` is safe (def bodies never contain `<<`; tagged blocks are single
`<` and `<\`). Two inherited details worth carrying:

- **Banner trivia (corpus-validated).** Decorative divider lines (`/*****…` with no
  closer, and bare `*****…`) are treated as trivia: an all-separator line
  (`*`/`/`/space only) is a section banner, skipped. A `/*` with *real content* and
  no closer is still a genuine `UnterminatedBlockComment`.
- **Greedy comment pairing (faithful, not a bug).** A `/*` pairs with the first
  `*/` anywhere after it (C rule). A `/*****` banner followed later by a normal
  `/* … */` would greedily swallow the gap — the old char parser did exactly the
  same, and the real corpus never triggers it.

### AST change: `Expr::Number(String)`

`Expr::Integer(i64)` / `Expr::Float(f32)` were merged into one raw
`Expr::Number(String)` (the sole intentional AST change; everything else —
`Definition`/`Statement`/`Spanned<T>`/`PathSegment`/`Call`/`Bool`/`String`/`Symbol`/
`Constructor`/`BitOr`/`Add` — is unchanged). Interpretation is type-specific per
field and lives in `reader.rs::Evaluator` (`i32`/`u32`/`f32`/…): a float-shaped
literal in an int context truncates; `f32` strips a trailing `f`; all asymmetries
(e.g. `f32` doesn't accept `NULL`) are corpus-driven and golden-verified.
`Expr::number_is_float` / `as_i32` / `as_f32` (in `def_text.rs`) classify and
convert for the sky-keyframe reader in `environment.rs`.

### Compile-diagnostics: two tiers, both authoritative

There is **no separate pre-lowering analysis pass** — it was removed as redundant
(§11 diagnostics, below). Two tiers own diagnostics, both rendered through **one
`codespan-reporting` chokepoint** in `build.rs`:

- **Parse errors** (`render_parse_error`) — strict, one per file. The headline is the
  *specific* reason (`Display` of the `TextParseErrorKind`: "unterminated string",
  "mismatched tag: opened <…>, closed <…>", "missing #end_definition"), not a generic
  "failed to parse". The renderer is **kind-aware**: `MissingEndDefinition` points at
  the unclosed definition; every other in-body error gets a caret at the offending
  byte plus an "in this definition" secondary label. (Before: any in-body error was
  mislabeled "missing #end_definition" and its real message discarded.)
- **Lowering errors** (`render_lowering_error`) — the authoritative, type-aware tier.
  `Evaluator::eval_*` attach the expression span; `LowerError::primary_span()`
  centralizes extraction. Covers eval/range/reference/unknown-symbol failures.

Integer-field type strictness landed (RT): references are `DefIndex`, numeric fields
evaluate strictly, and `EvalError::TypeMismatch { expected, found }` replaced the old
Debug-dump `UnexpectedExpression`. Unconsumed statements (misspelled field names,
cross-type specialization leftovers) now emit `Diagnostic::warning()` via
`codespan-reporting` with source spans and field-path extraction (§12.1).

---

## 12. Diagnostic gaps & fixes

The compiler is correct on valid input but **silent on invalid input** — the divergence
hunt proved every retail-valid def compiles correctly, but modders need feedback when
they make mistakes. Here are the gaps with exact code references and suggested fixes.

### 12.1 P0: Unconsumed statements — `drop_remaining` vs `finish()`

**Locations:** `reader.rs:823` (`drop_remaining`), `reader.rs:811` (`finish`), `lower.rs:1049` (call site).

A design tension exists: `DefReader` has **two** completion methods with opposite behavior:

- `finish()` — returns `Err(UnexpectedStatement(...))` if any statement was never consumed.
  Used by the dead lowering functions (§9 Track C) and bespoke arms that call `lower_generic`.
- `drop_remaining()` — silently discards unconsumed statements. Used by `lower_generic`.

This means a misspelled field name like `Healt 100;` in a def flowing through generic lowering
(~210 types) is a **silent no-op** — the field retains its NULLDEF default with zero diagnostic.
In defs flowing through bespoke arms that call `finish()`, it produces an error. **Inconsistent.**

**What's available at the call site:** Each unconsumed `Entry` holds a `&Spanned<Statement>`
which contains the full AST node with its wire-name path and source span. `DefReaderError::UnexpectedStatement(Spanned<Statement>)` already carries a span (though `primary_span()` returns `None` for it — add one).

**Fix:** Add a `remaining_statements()` method to `DefReader`:

```rust
pub fn remaining_statements(&self) -> Vec<&Spanned<Statement>> {
    self.entries.iter().filter(|e| !e.consumed).map(|e| e.stmt).collect()
}
```

Then in `lower_generic`, collect remaining and return them alongside the lowered body so
`build.rs` can emit `Diagnostic::warning()` for each one with its source span. This is ~50
lines of code. The cross-type specialization suppression (needed for templates that set
fields not present on all child types) is the only complexity.

### 12.2 P1: Bespoke-arm error swallowing (`.ok()` chains)

**Location:** `lower.rs` — **63 instances** of `.ok()` / `.unwrap_or(0)` / `.unwrap_or(-1)`
/ `.unwrap_or(0.0)` / `.unwrap_or(false)` / `.unwrap_or_default()` across **15 different
lowering functions**.

**Every single bespoke arm uses them.** The worst-hit areas:

| Arm | Lines | Count | What breaks silently |
|---|---|---|---|
| `build_animation_anims` | 1994-2090 | ~15 | `arg()`/`arg_f()`/`arg_b()` closures: typo in animation key → bank_index=0, anim_name=-1. An animation silently references the wrong bank. |
| `build_thing_components` | 2323-2356 | 2 | `env.def_string_offset(n)?` in filter_map: misspelled component name (`"CTCPhysicsStandar"`) → silently dropped |
| `lower_creature_def` (WoundMorphs/Expressions) | 2460-2530 | 6 | Bad expression type → entry silently skipped; morph field defaults to 0 |
| `CEntitySoundDef` | 2534-2588 | 8 | Missing key → pushed to overflow; sound indices default to 0 |
| `OPINION_REACTION_MANAGER` | 2859-3041 | 8 | `ctor_f32`/`ctor_i32`/`ctor_bool` closures: invalid ctor arg → 0.0/0/false |
| `OPINION_DEED_EFFECTS` | 2764-2812 | 5 | Bad opinion → statement passes to overflow; float fields default to 0.0 |
| `CHeroMorphDef` | 2633-2780 | 8 | Morph indices/values → 0/0.0/-1 |
| `CSpecialEffectsDef` | 3074-3109 | 2 | Bad key/value → entire `Add` entry silently dropped |
| `CFlammableDef` | 2590-2631 | 4 | Key/value pair silently skipped |
| `CWeaponDef` (trails) | 3128-3168 | 3 | Augmentation symbol → 0; trail graphic indices → 0 |
| `CDegradableDef` | 3188-3225 | 5 | Health/bank/particle/navigation → defaults |
| `CReplaceableMeshDef` | 3227-3263 | 2 | Graphic type/bank → 0 |
| `CAppearanceModifierDef` | 3265-3308 | 3 | Int/float args → 0/0.0 |
| `build_tex_morphs` (RandomAppearanceMorph) | 2210-2250 | 3 | Morph args, mesh id, texture indices → 0 |
| `apply_struct_from_expr` | 1573-1680 | 1 | `num_pairs` extraction |

**Fix approach:** The lowest-friction change is replacing `.ok()` / `.unwrap_or(0)` with explicit
error propagation. For closure patterns like `arg = |...| ...`, collect warnings into a shared
`&mut Vec<Spanned<Statement>>` at the `build.rs` level and emit as diagnostics. For the
common "missing → push to overflow" pattern (CEntitySoundDef, CSpecialEffectsDef),
emit a warning before pushing to the filtered body. This is ~500 lines of changes but high
value for modder safety.

**Minimal quick-win:** For `build_thing_components` specifically (`lower.rs:2347`): if
`def_string_offset` returns `None` for a component name, emit a diagnostic and continue —
this catches the "misspelled component name" case which is the most likely modder mistake.

### 12.3 P2: Sub-def lowering failures are warnings, not errors

**Location:** `build.rs:662-673`

```rust
Err(e) => {
    let diag = Diagnostic::warning()   // <-- WARNING
        .with_message(format!("sub-def lowering failed for <{tag}> in {name}"))
    // ...
    sub_fail += 1;
    continue;                           // <-- skips, build succeeds
}
```

The `sub_fail` counter (line 672) is **purely informational** — printed in the summary but
never triggers a non-zero exit. The named-def equivalent (`error_count` at line 317) DOES
cause `Err(...)` return. This is an inconsistency.

**Fix:** Change `Diagnostic::warning()` to `Diagnostic::error()` and return `Err(...)` if
`sub_fail > 0`. A 3-line change. First verify the current corpus produces zero `sub_fail`
(it should — all sub-defs lower correctly against the corpus).

### 12.4 P2: `from_i32_or_first` — out-of-range enum silently mapped to 0

**Location:** `lower.rs:65-68`, used only at `lower.rs:269-273` in `lower_action_input_control`.

```rust
fn from_i32_or_first<T: DefEnum>(value: i32) -> T {
    T::from_i32(value).unwrap_or_else(|| T::from_i32(0).unwrap())
}
```

Affects 5 fields: `game_action`, `controller_type`, `keyboard_key`, `xbox_button`, `mouse_button`.
An out-of-range enum value silently becomes variant 0. The generic lowering path already
correctly errors on out-of-table enums (`lower.rs:1178-1184`).

**Fix:** Replace with `Result`-returning helper and error propagation (5 lines changed).

### 12.5 P3: Integer narrowing without range check

**Location:** `lower.rs:1200-1224` and `lower.rs:1290-1294` — **10 narrowing casts** in generic
paths (8 in `visit_field`, 4 in `apply_expr` for `U8`/`U16`/`I8`/`I16`).

```rust
FieldRef::U8(slot) => { *slot = v as u8; }  // 300 → 44, silently
```

**Fix:** Add range checks before each cast (~40 lines). For enum-derived casts in bespoke arms
(CDegradableDef `lowered.graphic_type.to_i32() as u8` — 2 instances, safe), no change needed.

### 12.6 P3: Other gaps

- **Duplicate def names**: ~800 duplicate instance names silently overwrite in
  `collect_named` and `mod.rs`'s per-file `by_name`. Scan for collisions and emit a warning
  per duplicate. **Still open** — §15 once listed this as done; there is no such code and a
  stock build emits zero duplicate-name warnings.
- **Header failures**: ~~silently skipped~~ **fixed** — a header that fails to read, parse,
  or evaluate is now `Severity::Error` and fails the build at the header, in both
  `load_symbols` and the `.def`-file-local `evaluate_items` path. Previously these were
  warnings, so one malformed header turned into thousands of "unknown symbol" errors at
  distant use sites (measured: a single `0x`-hex enum value produced 7,336). See §4.1.1.
- **Missing `#end_definition` recovery**: handled at parse time (correct).

---

## 13. Enum typing

Many def fields are typed as plain `i32`/`u32`/`u8` when existing `DefEnum`/`DefFlags`
types could be used directly. Since `DefEnum` and `DefFlags` both serialize as `i32` LE,
all of these changes are **byte-identical** — golden stays green, no re-bless.
Additionally, 2 fields are mistyped as `i32` when they should be `DefIndex`.

### 13.1 Priority 1: Existing enums, ready to apply

**UiDef** (`packages/defs/src/def/ui.rs`) — 21 fields (all done):

| Field | Current | Replace with | Status |
|---|---|---|---|
| `type_` | `i32` | `UiType` (~30 variants) | ✅ Done (byte-identical; `lower_ui` updated) |
| `expansion_type` | `i32` | `TableExpansion` (DefFlags) | ✅ Done |
| `mesh_type` | `i32` | `EngineGraphicType` | ✅ Done |
| `alignement` | `i32` | `TextAlignment` (LEFT/CENTER/RIGHT) | ✅ Done |
| `sprite2_d_flag` | `i32` | `Sprite2dFlags` (DefFlags) | ✅ Done |
| `action` | `i32` | `ActionType` (~300 variants) | ✅ Done (gaps 32–35/217–218/290 verified unused) |
| `action_on_back` through `action_on_left_clicked_under` | `i32` × 15 | `ActionType` | ✅ Done |

**Other fields with existing enums:**

| File | Field | Current | Replace with | Status |
|---|---|---|---|---|
| `ui_state.rs:26` | `state_change_type` | `i32` | `StateChangeType` (5 variants) | ✅ Done |
| `inventory_item.rs:48` | `tutorial_category` | `i32` | `TutorialCategory` (~95 variants) | ✅ Done |
| `gift.rs:6` | `gift_type` | `i32` | `GiftType` (FRIENDLY/ROMANTIC/OFFENSIVE) | ✅ Done |
| `inventory_item.rs:26` | `use_button_action` | `i32` | `GameAction` (~128 variants) | ✅ Done |

**Total done: 26 fields, all byte-identical.** 22 fields needed bespoke lowering code updated in `lower_ui`/`apply_ui_state`; 4 fields were already generic.

**Not candidates** (removed from §13.1 after investigation):
- `melee_combat_knockdown_effects.rs` `attacker_lighting_channel` / `target_lighting_channel` — decomp `Transfer<long>` not `Transfer<ELightingChannel>`; -1 sentinel has no enum variant. False positive — these are correctly `i32`.

**Deferred** (blocked by u8 wire-size mismatch — WireStruct positional layout changes):
- `degradable.rs:17` DegradableInfo `type_: u8` — EngineGraphicType wire size is 4 bytes (i32)
- `replaceable_mesh.rs:14` ReplaceableMeshesEntry `graphic_type: u8` — ditto

### 13.2 Priority 2: DefIndex mistypes (byte-identical, but semantically wrong)

Two fields are `i32` where all siblings/namesakes use `DefIndex`:

| File | Field | Current | Should be | Evidence |
|---|---|---|---|---|
| `creature.rs:39` | `magic_screen_inventory` | `i32` | `DefIndex` | All other `*_screen_inventory` fields in `CreatureDef` are `DefIndex` (`map_screen_inventory`, `stats_screen_inventory`, `experience_screen_inventory`…) |
| `trap.rs:32` | `physical_obstruction_def_index` | `i32` (default -1) | `DefIndex` | Named "def_index"; sibling field `explosion_def_index` is `DefIndex` |

These are **byte-identical** (both serialize as i32 LE) but semantically wrong. Re-typing
them to `DefIndex` lets the differ resolve the reference by name, de-noising the ledger.

### 13.3 Priority 2: New DefFlags types (need decomp extraction)

| Files (10) | Field | Current | Needs |
|---|---|---|---|
| `thing_base.rs`, `thing_creature.rs`, `thing_object.rs`, `thing_village.rs`, `thing_building.rs`, `thing_noise.rs`, `thing_holy_site.rs`, `thing_switch.rs`, `thing_marker.rs`, `thing_physical_switch.rs` | `persistence_flags` | `i32` (default `1`) | `#[derive(DefFlags)] PersistenceFlags` with `EPF_STATIC=1`, etc. from decomp |
| `physics.rs:12` | `interaction_flags` | `i32` | `InteractionFlags` DefFlags from decomp |
| `creature_mode.rs:6,8,10` | `default_creature_mode`, `initial_creature_mode`, `default_weapon_creature_mode` | `i32` | `CreatureMode` DefEnum |
| `creature.rs:53` | `creature_group` | `u32` | `CreatureGroup` DefFlags (bitmask) |
| `feat.rs:34` | `kn_creature_type` | `i32` | `CreatureGroup` DefFlags (same bitmask) |
| `tattoo.rs:24,34`, `appearance_modifier.rs:13` | `covers_body_area_flags`, `specific_covers_body_area_flags` | `i32` × 3 | `BodyAreaFlags` DefFlags |
| `hit_location.rs:30` | `flags` | `i32` | `HitLocationFlags` DefFlags |
| `occupiable.rs:6` | `type_flags` | `u32` | `OccupiableTypeFlags` DefFlags |

**Total: ~19 additional fields across 18 files — need decomp extraction.**

### 13.4 Not candidates

Fields that look enum-like but are actually text IDs, graphic bank IDs, state indices,
frame counts, or numeric thresholds were systematically checked and excluded. See
the comprehensive audit report for the full list of false positives.

---

## 14. In-game testing plan

The compiler output should be tested against the actual game engine. Below are the
categories of testing, ordered by risk.

### 14.1 Correctness verification (playthrough)

Load a savegame with the compiled binaries and verify:
- **Save/load** works (already verified)
- **Region transitions** work (script defs trigger correctly)
- **Combat** works (weapon damage, particles, trails)
- **Creature behavior** works (animations, expressions, wound morphs)
- **UI** works (all screens, map paths, text display)
- **Sound** works (music changes, sound effects)

### 14.2 Bespoke-lowering stress tests (high-risk areas)

These are the most complex lowering arms — test their effects in-game:
| Feature | Def type | What to test |
|---|---|---|
| Weapon augmentations | CWeaponDef, CObjectAugmentationsDef | Apply flame/lightning/silver augmentations; verify particles appear. Remove augmentations; verify particles stop. |
| Creature animations | CAppearanceDef | Watch creature animations in combat and idle. Verify transitions, combos. |
| Creature wound morphs | CCreatureDef | Damage creatures heavily; verify wound textures appear on correct body parts. Wait for scars to form. |
| Hero morphs | CHeroMorphDef | Apply haircuts, tattoos, scars. Verify BLEND_ALPHA=2 makes them render correctly. |
| Entity sounds | CEntitySoundDef | Listen for creature vocalizations, footstep sounds, attack grunts. |
| Opinion system | OPINION_DEED_EFFECTS, OPINION_REACTION_MANAGER | Watch NPC reactions to hero's alignment. Verify deed effects (fear, love) apply correctly. |
| Degradable objects | CDegradableDef | Destroy barrels, crates, doors. Verify degradation stages and particle effects. |
| UI interactions | UiDef, FRONT_END | Navigate all menus; verify controller and keyboard input on all UI types. |

### 14.3 Modification testing (modder workflow)

The goal is to verify that modifying defs produces predictable in-game changes:

1. **Simple scalar**: change `OBJECT_IRON_LONGSWORD.Damage` from 3 to 50. Verify one-hit kills.
2. **Reference**: change `OBJECT_IRON_LONGSWORD.Property` to `WP_FLAME`. Verify fire damage type.
3. **Enum value**: change `OBJECT_IRON_LONGSWORD.Type` to `WT_AXE`. Verify axe animations/behavior.
4. **Add a new weapon**: copy `OBJECT_IRON_LONGSWORD`, change its name and mesh. Verify it appears.
5. **Modify a creature**: change `CREATURE_BANDIT_01.Health` from 100 to 1. Verify one-hit kill.
6. **Modify a UI element**: change a button's `Action` to a different action. Verify new behavior.
7. **Intentional error**: misspell a field name. Verify the binary still loads (field silently
   dropped). **This should become a diagnostic after §12.1 is implemented.**

### 14.4 Enum validation test

After §13 changes land (byte-identical), verify that:
1. Golden test still passes
2. Verity ledger shows no new Bug or AcceptArtifact entries
3. In-game: changing a field to an out-of-range enum value produces a compiler error (e.g.
   setting `DamageType` to 999 in a weapon should fail, not silently become variant 0)

---

## 15. Release checklist

### v1.0 blockers

- [ ] **In-game load test** — deploy current output to `~/Fable/data/CompiledDefs/`,
      load a savegame, verify no crashes in 5+ minutes of gameplay
- [x] **drop_remaining warning infrastructure** (§12.1) — `remaining_statements()` added
      to `DefReader`, warnings vec piped through `lower_def`/`lower_generic`, and wired
      to `Diagnostic::warning()` in `build.rs` for both named and sub-def lowering
- [x] **Enum typing P1** (§13.1) — 26/29 candidate fields done (byte-identical).
      2 LightingChannel fields are false positives (decomp `Transfer<long>`, -1 sentinel).
      2 fields (DegradableInfo/ReplaceableMeshesEntry) deferred: u8 WireStruct needs
      EngineGraphicType-as-u8 adapter.
- [x] **DefIndex mistypes** (§13.2) — 2 fields fixed (`magic_screen_inventory`, `physical_obstruction_def_index`)
- [x] **Golden passes** after all changes

### v1.0 recommended

- [ ] CI pipeline (`.github/workflows/ci.yml`)
- [ ] In-game modification tests (§14.3) — prove def modifications work end-to-end
- [x] Dead code cleanup (§9 Track C) — deleted 4 unused lowering functions

### v1.1 deferred

- [x] Bespoke-arm error propagation (§12.2) — key arms covered: `build_animation_anims`,
      `CEntitySoundDef`, `CSpecialEffectsDef`, `OPINION_DEED_EFFECTS`; closure-heavy
      arms (`OPINION_REACTION_MANAGER` etc.) retain `.unwrap_or(0)` with comment
- [x] Sub-def failure strictness (§12.3) — sub-def lowering failures now errors,
      `sub_fail > 0` blocks build
- [ ] Enum typing P2 (§13.3) — needs decomp extraction first
- [x] P3 diagnostics (§§12.4-12.6) — `from_i32_or_first` replaced with strict helper;
      integer narrowing range checks added; header failures now **errors** (§4.1.1)
- [ ] Duplicate def name warnings (§12.6) — **not implemented**; this line previously
      claimed ~1,161 warnings were emitted, but no such code exists and a stock build
      emits zero. Left open rather than silently dropped.
- [x] Header-set variant resolution + non-fatal duplicate symbols (§4.1.1) — fixes mods
      being silently ignored when they patch `RetailHeaders/`
- [ ] Unit tests for lowering (§9 Track C)
- [ ] Quality (R8 object.rs, R6 assembly, R4/R5 schema-ization)
