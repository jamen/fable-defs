# fable-defs — Def Compiler: Architecture & Handoff Guide

fable-defs is a **Rust** workspace providing a **from-scratch def compiler** (`defc`) that
compiles the text `.def` sources from Fable: The Lost Chapters' debug build into the retail
binary format: `game.bin`, `frontend.bin`, `script.bin`, and the shared `names.bin`.

**Where it stands:** all four binaries compile entirely from text — no retail binary is read
at build time — and the retail engine loads and runs the output (save-load verified). Golden
byte-determinism holds, the workspace is warning-clean, and the semantic ledger (§6) surfaces
only verified artifacts. Divergence closure is done; the text layer has one unified grammar;
diagnostics point at real source with real spans.

This document is the architecture reference *and* the operational handoff. Keep it current.
It deliberately records **decisions and their reasons**, not project history — if you want to
know what changed when, read the git log.

---

## Contents

1. [Orientation](#1-orientation)
2. [Pipeline](#2-pipeline)
3. [Central design fact: typed structs are the IR](#3-central-design-fact-typed-structs-are-the-ir)
4. [Layer guide](#4-layer-guide)
5. [Diagnostics](#5-diagnostics)
6. [Verification & the bug-fixing loop](#6-verification--the-bug-fixing-loop)
7. [The manifest](#7-the-manifest)
8. [Settled decisions](#8-settled-decisions)
9. [Roadmap](#9-roadmap)
10. [Operational reference](#10-operational-reference)

---

## 1. Orientation

### Workspace crates

| Crate | Type | Role |
|---|---|---|
| `defs` | lib | Format library: text parsing, the typed def model, binary I/O, the `SemVal` differ |
| `defs-derive` | proc-macro | The five schema derives (`DefStruct`/`WireStruct`/`DefVariant`/`DefEnum`/`DefFlags`) |
| `def-compiler` | lib | Lowering (text → typed structs) and the whole build pipeline |
| `defc` | bin | CLI front end: argument parsing, progress output, diagnostic rendering, exit code |
| `def-compiler-sys` | cdylib | C ABI for downstream consumers (EgoCore); renders diagnostics to a plain-text log |

### Key external resources

| Resource | Path |
|---|---|
| Fable decompilation | `~/git/fable-decomp` — the `Transfer<T>` type/order oracle (§6). **Absent at time of writing** |
| Retail binaries (ground truth) | `~/Fable/data/CompiledDefs/backup-retail-verified/` |
| Anniversary debug build (text defs) | `~/doc/Fable_Anniversary-2013-02-25/Fable/Data/` |
| Live game install (ships **both** header sets, §4.1) | `~/Fable/data/Defs/` |
| EgoCore (downstream; links `def-compiler-sys`) | `~/git/EgoCore` — its C++ `FableDefCompiler/` is a behavioural oracle |

```bash
REF=~/Fable/data/CompiledDefs/backup-retail-verified
TEXT=~/doc/Fable_Anniversary-2013-02-25/Fable/Data
cargo run -q -p defc -- $TEXT/Defs <out_dir>
```

### Scale (measured, release build, Anniversary corpus)

| | |
|---|---|
| Source files parsed | 177 (`.def`/`.tpl` + `.h`) |
| Definitions | 10,454 |
| Output entries | game 13,239 · frontend 851 · script 611 |
| game.bin composition | NULLDEFs + 9,264 named + 3,726 distinct anonymous sub-defs (33,525 lowered, deduped) |
| **Total build** | **3.5 s** |
| — parse, all files | 0.35 s (10%) |
| — **lower + emit game.bin** | **3.09 s (88%)** |
| — lower + emit frontend + script | 0.06 s (2%) |

Re-measure with the timing recipe in §10 before optimising anything; these numbers drive the
incremental design in §9.

---

## 2. Pipeline

```
 INPUTS                     defs (format layer)             def-compiler
┌──────────────┐   ┌──────────────────────────────────┐   ┌────────────────────────┐
│ Defs/*.def    │──▶│ parse_source → SourceAst {        │──▶│ flatten_specialization │
│ Defs/*.tpl    │   │   items: Vec<Item> }              │   │  (parent-first concat) │
│ Defs/**/*.h   │──▶│   Item = Definition | Enum |      │   │ lower_def dispatch:    │
│               │   │     Define | Namespace |          │   │  • ~35 bespoke arms    │
│ manifest.rs   │   │     Conditional | PragmaOnce      │   │  • lower_generic via   │
│ (membership)  │   │ SymbolTable::evaluate_items       │   │    VisitFields         │
└──────────────┘   ├──────────────────────────────────┤   └────────────────────────┘
                    │ ~273 DefStruct + 86 WireStruct +  │                │
                    │ 4 DefVariant + 62 DefEnum +       │◀───────────────┘
                    │ 13 DefFlags = THE TYPED IR        │
                    │ binary.rs: container + SemVal     │   ┌────────────────────────┐
                    └──────────────────────────────────┘   │ build.rs: per binary   │
                                                            │  • collect_named       │
 VERIFICATION: golden.rs (byte determinism — THE gate)      │  • NULLDEFs            │
              · verify.rs semantic ledger vs retail         │  • sub-def tables      │
              · in-game load                                │  • NamesBuilder        │
                                                            │  • 16 KB zlib chunks   │
                                                            └────────────────────────┘
```

**Stage 0 — inputs.** Text corpus (`.def` = concrete `#definition`s, `.tpl` = abstract
`#definition_template`s; both must load so `specialises` chains resolve), C-like header files,
and the static manifest (per-binary membership + NULLDEF lists, extracted once from retail).

**Stage 1 — parsing** (`defs::text`). One grammar, one parser, one AST for every file kind
(§4.1). A definition body is three statement forms:

```text
Health 100;                            → Statement::Field   (path, expr)
Controls.Add(CActionInputControl(…));  → Statement::MethodCall (object path, call)
<CTavernGameDef> … <\CTavernGameDef>   → Statement::TaggedBlock (tag, body)
```

**Stage 2 — specialization flattening** (`lower::flatten_specialization`). The `specialises`
chain concatenates most-distant-ancestor-first into one statement list. With downstream
last-wins semantics this reproduces the game compiler's copy-parent-then-apply. Same-tag
tagged blocks **merge** (parent first), never replace.

**Stage 3 — lowering** (`def-compiler::lower`). `lower_def` dispatches on def type name.
~35 bespoke arms handle C++-specific logic (§4.5); the remaining ~210 flow through
`lower_generic::<T>`.

**Stage 4 — assembly** (`def-compiler::build`). Per binary: allocate a global index space
(NULLDEF region first, then named entries in first-seen corpus order, filtered by manifest),
lower every body, generate sub-def tables + deduplicated anonymous entries (game.bin only),
intern all strings into one shared `NamesBuilder`, pack into ≤16 KB zlib chunks, serialize.

**Stage 5 — serialization** (`defs::binary` + `wire`). Every typed struct serializes itself;
the container layer adds headers, name-ref tables, the chunk index, and compression.

---

## 3. Central design fact: typed structs are the IR

**The typed def structs are the compiler's IR**; the pipeline is `text AST → typed structs →
bytes`. Each `#[derive(DefStruct)]` struct is simultaneously (1) the binary layout spec,
(2) the parse target when reading retail bins, (3) the lowering target when compiling text,
(4) the reflection subject for generic lowering and the differ. **Layout is declared exactly
once**, so parse/serialize/size can never disagree, and golden pins the whole chain.

Proportions: ~4.5k lines of logic over ~22k lines of declarations. Most of the repo is schema,
not machinery — the right shape for a format compiler, and the reason most of the backlog is
about moving *more* knowledge from machinery into schema.

**Rejected alternatives** (don't relitigate): a mid-level "resolved statement" IR between text
AST and typed structs (adds a layer for the ~210 types that don't need one, changes nothing
for the ~35 bespoke arms whose logic is irreducible retail-compiler knowledge); a
dynamic/schema-interpreted def model (loses compile-time layout checking, makes every bespoke
arm stringly-typed).

---

## 4. Layer guide

### 4.1 Text layer (`defs/src/text/`)

| File | Contents |
|---|---|
| `lexer.rs` | `lex(input, file)` → `Vec<Token>`; `Cursor`; `TokenKind`; `TextParseErrorKind` |
| `mod.rs` | **The whole grammar**: `Item`, `SourceAst`, the AST types, and every production |
| `symbols.rs` | `SymbolTable::evaluate_items` — declarations → flat `name → i64` |
| `base.rs` | `FileId`, `Span`, `Spanned<T>`, `LineIndex`, `ParseContext`, `ParseError<T>` |

**One grammar for every file under `Defs/`.** There is no separate header parser. A file —
`.def`, `.tpl`, or `.h` — is a sequence of `Item`s: definitions *and* declarations
(`enum`, `#define`, `namespace`, `#ifdef`/`#ifndef`, `#pragma once`).

> **Why:** the split was conventional, not structural. `engine_local_detail.def` declares two
> `enum`s whose 31 symbols appear in no header and are referenced ~1,800 times, so a `.def`
> legitimately carries declarations. Worse, the two parsers had different top-level item sets
> and opposite strictness, so **the same text meant different things depending on extension**:
> `#ifdef` was a real conditional in a `.h` and silently-skipped junk in a `.def`, which meant
> a guarded `#define` in a `.def` leaked its symbol unconditionally.

File *discovery* still keys off the extension — header-set variant resolution applies to `.h`,
and only `.def`/`.tpl` contribute definitions. Only the grammar is unified.

Load-bearing properties:

- **Spans carry their file** (`Span { file: FileId, start, end }`). Non-negotiable:
  `flatten_specialization` clones a template's statements into every inheriting definition,
  spans included, so a span routinely outlives the file it was read from. Without the file id
  a diagnostic interprets the offset against the wrong text and points confidently at
  unrelated code. There is **deliberately no `Span::join`** — combining spans is only
  meaningful within one file and nothing can guarantee that; productions needing a multi-token
  range use `Span::new(cursor.file(), start, end)`, taking the file from the cursor.
- **AST nodes carry spans** (`Spanned<Statement>`, `Spanned<Expr>`, `PathSegment::Index`,
  `Item::Definition(Spanned<Definition>)`, and all declaration names). `Spanned<T>: PartialEq`
  ignores the span. Corpus files are CRLF; `\r` is trivia.
- **Statement order is preserved** through parsing and flattening — load-bearing, because
  method-call DSLs (`Animation.StartGroup` … `Add` … `EndGroup`) and `Field.clear()` have
  positional semantics.
- **A valueless `#define NAME` binds no symbol.** 46 of them in the corpus (every include
  guard) against 25 with values. `Define.value` is `Option<i64>`; in C, using a valueless
  define as a number is an error too, and this keeps guard names out of the table entirely.
- **Include guards are ordinary conditionals** and evaluate correctly on their own, so there
  is no prologue/epilogue special-casing. `#endif __GUARD__` (the pre-C99 idiom) is accepted.
- **`SymbolTable` duplicates: last definition wins**, never fatal. Matches the C preprocessor
  and the retail tooling's `m_SymbolMap[name] = value`. `evaluate_items` returns
  `Vec<Redefinition>` that the builder reports as located warnings.
- **Text outside any item is skipped, recorded, and not reported.** `SourceAst.ignored` holds
  the coalesced byte ranges. The canonical corpus has four such runs (leftover bodies of two
  commented-out definitions, a stray identifier, a duplicate `#end_definition`) — see §5 for
  why nothing warns about them today.
- **Decorative banners are lexer trivia.** An all-separator line (`/*****…` with no closer
  anywhere after it) is a section divider, not a comment. This *cannot* be replaced by
  "ignore text outside definitions": it is a lexer concern and "outside a definition" is a
  parser notion. Measured: 65 firings, all outside definitions, none in headers. A `/*` with
  real content and no closer is still `UnterminatedBlockComment`.
- **Greedy comment pairing is faithful, not a bug.** A `/*` pairs with the first `*/` anywhere
  after it (the C rule). The old char parser did the same and the corpus never triggers the
  pathological case.
- **`Expr::Number(String)` keeps literals raw.** Interpretation is type-specific per field and
  lives in `reader.rs::Evaluator`: a float-shaped literal in an int context truncates, `f32`
  strips a trailing `f`. All asymmetries are corpus-driven and golden-verified.

#### Header-set variant resolution

`Data/Defs` does **not** ship one flat set of headers. It ships *complete variants of the same
set*, along two axes:

| Axis | Variants | Resolution |
|---|---|---|
| Build | `RetailHeaders/` vs `DevHeaders/` | `HEADER_SET_ROOTS`, most-preferred first; only the winner is scanned |
| Platform | `pc/` vs `xbox/` within a set | `IGNORED_PLATFORM_DIRS` skips `xbox` |

`RetailHeaders` wins: it is the richer set (the `DevHeaders` lipsync headers are empty stubs)
and it is what the modding tools write to. The two agree on every shared name/value pair, so
the choice is byte-neutral for a stock corpus — verified three ways. It decides only *which
set a mod's edits are read from*. The Anniversary corpus ships only `DevHeaders/`. When a set
is skipped the build warns, naming it.

> **The bug this prevents:** both sets were once scanned; `DevHeaders/*` sorted first and
> claimed every symbol; each `RetailHeaders/*` file then hit a duplicate on its *first* enum
> variant and — because a duplicate aborted the rest of the file — was discarded whole.
> ~44,900 symbol definitions dropped at warning severity. A mod that patched `RetailHeaders/`
> saw its five new symbols surface as one `unknown constant` error 5,279 lines away. Two
> independent defects: fatal duplicates, and unioned variant sets. Both fixed; regression
> tests in `build.rs::tests` and `symbols.rs::tests`.

### 4.2 Schema layer — proc-macro derives (`defs-derive`)

| Derive | Instances | Generates |
|---|---|---|
| `DefStruct` | ~273 | `Default`/`DefDefault` + control-level `parse`/`serialize`/`byte_size` + `visit_fields` + `Wire` + `StructSlot` (`visit_named=true`) + `AsField` |
| `WireStruct` | 86 | compound value (members, **no** control ids) |
| `DefVariant` | 4 | tagged union (`u32` tag + case fields) |
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
**polymorphic field** whose in-memory variant is chosen by an earlier-declared sibling's value
(`UiDef.mesh_index`/`MeshRef`).

**Wire encodings** (byte-verified against every entry of all three retail bins): `f32`/`i32`/
`u32`/`u16`/`u8` LE; `bool` 1 byte; `String` NUL-term UTF-8; `WStr` NUL-term UTF-16LE;
`PString` u32 len + bytes; `DefString` i32 names.bin offset (default −1); `DefIndex` i32 global
index (default 0); `Vec<T>` u32 count + elems; `[T;N]` N elems no count; `BTreeMap` u32 count +
key-ordered pairs (`std::map`); `VecMap` same shape, stored order preserved (`CVectorMap`);
`DefEnum` 4 bytes must be in table; `DefVariant` u32 tag + case fields.

A `DefStruct` body is a sequence of **field controls**: `u32 crc32(wire name)` + wire value, in
declaration order. `crc32` ids are **validated, not dispatched**. The `DefBody` enum and its
dispatch are generated by a local `def_body!` macro in `binary.rs` from an inlined table of
~247 rows. **Proc macros can't enumerate a crate's types, so this central list is irreducible.**

### 4.3 Reflection layer (`defs/src/visit.rs`)

`FieldRef<'a>` is a typed mutable handle to one field (one variant per wire kind, plus
`Vec`/`Map`/`Struct`/`Variant`/`Array` slot traits and a `Complex` escape hatch).
`visit_fields` pushes each field with its **wire name**. Consumers: generic lowering (mutating
walk) and the semantic differ (reading walk). Known limits, all from the `&mut`-only design:
`MapSlot::for_each_pair` clones keys/values to read them; three overlapping surfaces.

### 4.4 Lowering engine (`def-compiler`)

Three primitives in `reader.rs`:

- **`Evaluator`** — single source of truth for evaluating one `Expr` against the symbol table.
  Handles `NULL` (→0), `TRUE/FALSE/BTRUE/BFALSE`, `|` and `+` folds. Deliberate asymmetries
  (e.g. `f32` doesn't accept `NULL`) reflect the corpus — extend only with corpus evidence.
- **`Args`** — positional reader over ctor/method-call argument lists.
- **`DefReader`** — scans statements by name at a path depth; on duplicates **last wins**
  (specialization concatenates parent+child, so last-wins realizes child-overrides-parent).
  Combinators: `group`, `indexed_sparse`, `keyed`, `calls`, `any_*`, `finish`.

**Pull model.** The obvious "iterate statements, look up field by name" is blocked by the
borrow checker (can't hold a `name → FieldRef` map — aliasing `&mut`). So `lower_generic`
clones the NULLDEF base and an `Applier` (a `FieldVisitor`) walks fields via reflection, each
field *pulling* its matching statements. Order-sensitive semantics that don't survive inversion
are handled by a pre-pass (`strip_superseded_by_clear`) or by interception before the generic
walk (method-call DSLs).

**Unconsumed statements are silently dropped.** This matches C++ `Transfer` skipping unknown
field names, which cross-type specialization needs. There is no warning: see §9 for why the
previous attempt was removed and what reviving it would require.

`DefReader::finish()` is different and **is** live — it validates a *group* sub-reader (a Vec
element, map value, or indexed sub-struct), which really does own every statement handed to it.

### 4.5 Bespoke lowering catalog

Everything the generic walk can't do lives in ~35 arms in `lower_def`. The decomp is the
authority.

**A. Method-call DSL interception** (statement-order replay of a C++ builder API):
`CAnimatingObjectDef`/`CAppearanceDef` (`Animation.Add/StartGroup/…`, key=crc(name),
stable-sorted); `CCreatureDef` (`Expressions.Add`, `WoundMorphs.*`, 40-byte packets);
`CEntitySoundDef` (`SoundMap.*`, crc-keyed, sorted after each add); `CFlammableDef`
(`std::map<CDefString,…>` ordered by TablePos not strcmp); `CHeroMorphDef`;
`OPINION_DEED_EFFECTS` (seconds→frames ×15, **x87 single-rounding** via f64 intermediates);
`OPINION_PERSONALITY` (5×36-byte blob, args 6/7 swap into slots 7/6);
`OPINION_REACTION_MANAGER` (ctor families precompute inverse radii / per-frame rates,
hurried-set at idx+79); `CSpecialEffectsDef` (crc-keyed, dup insert = no-op **first** wins,
then sort); `CDegradableDef`; `CReplaceableMeshDef`; `CAppearanceModifierDef` (24-byte packet,
args reordered); THING family ×10 (`Components.Add/Remove`, one `lower_thing!` macro,
DriverType from the static 13-entry registry, universal `trailing_u32=28_011_726`).

**B. Container-semantics fixups** (post-walk): `CCombatAbilityBlock*` (`ValidBlockWeaponTypes`
is a `std::set` → sort+dedup); `PLAYER_GUI` (`VecMap` sorted by crc32(key));
`OPINION_REACTION_MANAGER` (eight `VecMap` fields sorted by key).

**C. Quirks:** `CAMERA_MANAGER` (consume `CameraList.clear()`); `CWeaponDef` (`WeaponTrails`
stored swapped); `CWillResponseDef` (strip Anniversary-only `ForceLightningable`);
`OPINION_SOURCE` (derive 79 bools from flag defaults+maps); `CLookDef.CombatMaxTurnSpeed`
(naive default propagation regresses ~503 entries — 1-entry divergence accepted, NOT
implemented).

**D. Compound ctor arg permutations** (in `apply_struct_from_expr`): `BlendedParticleEffectSet`,
`ObjectAugmentationParticleSet`, `ExplosionRing` (0/1 swap), `ParticleAttachmentInfo` (0/1
swap), `AttackHistoryCombo` (multiplier-first vs vec-first).

**E. Hand-written frontend lowerings:** `UI`, `CONTROL_SCHEME` (`CActionInputControl` slot
dispatch by controller type), `UI_MISC_THINGS_DEF`, `FRONT_END`. Other frontend types
(`ENGINE`, `ENVIRONMENT`, `ENVIRONMENT_THEME_DAY`, `ENGINE_VIDEO_OPTIONS`,
`CONFIG_OPTIONS_DEFAULTS_DEF`, `UI_ICONS_DEF`) flow through generic.

**~39 `.ok()` / `unwrap_or` sites remain** in these arms, where a bad constructor argument
silently becomes `0`/`-1`/`false` instead of erroring. See §9.

### 4.6 Assembly layer (`def-compiler/src/build.rs`)

One pipeline, three instantiations (game/frontend/script differ only in file collection,
manifest slices, and the game-only sub-def region):

1. **Index allocation** (`collect_named`): global index = position. NULLDEF region first, then
   one index per distinct named def in first-seen corpus order, filtered by `*_NAMED`. File
   order = sorted path list. ~800 duplicate instance names exist and **the whole later body
   replaces the earlier one** (last-processed-file wins), matching retail's
   `CompileDefinition`. The *first* occurrence keeps the index slot; the *last* supplies the
   body. No diagnostic (§9).
2. **NULLDEF bodies**: `lower_def(class, empty)` = the type's `def_default()`. Preamble
   `(false,false,0)`.
3. **Named bodies**: flatten → lower. Preamble `(true,false,1)` — the `1` is
   `AreDefaultValsApplied`, load-bearing (0 ⇒ save-load crash). **No fallback**: a def that
   fails to lower is not emitted.
4. **Sub-defs (game.bin only)**: merge same-tag tagged blocks across the specialization chain
   (CRC of tag, parent-first concat), lower each, serialize, dedup anonymous entries by
   **(class-tag, body bytes)** — byte-only dedup type-confuses combat sub-defs and crashes
   save-load. Anonymous `file_name_offset = 0xFFFFFFFF`.
5. **names.bin**: one shared `NamesBuilder` across all three; header words at +8/+12 are
   content-derived.
6. **Chunking**: greedy split at 16 KB decompressed, zlib level 1 (`78 01`).

> `flatten_specialization` currently runs **twice per definition** — once in
> `emit_nulldef_and_named`, once in `build_subdefs`. Cheap win available (§9).

### 4.7 Binary container layer (`defs/src/binary.rs`)

```text
DefBinary   := header(13B) · NameRef[n](12B) · ChunkIndexHeader(8B)
             · ChunkIndexEntry[chunks](8B) · sentinel ChunkIndexEntry (REQUIRED —
               chunk offsets are relative to the region after it) · zlib chunks
NameRef     := def_name_offset(u32) · file_name_offset(u32) · ClassIndex(u32)
EntryRecord := preamble(3B: is_real, is_template, AreDefaultValsApplied)
             · [sub-def table: u16 count · 12B records]  — iff the type derives the sub-def bases
             · body: field controls (u32 crc32(name) · wire value)…
names.bin   := 20B header (off8=StringCount, off12=StreamLength) · (u32 crc · NUL utf8)…
```

- **`NameRef.ClassIndex` is load-bearing and the differ cannot see it.** It is
  `CInstantiatedDefInfo::ClassIndex`: **one 0-based counter per class**, running continuously
  in global-index order across all three emission passes — NULLDEF first, then named, then
  anonymous sub-defs. `CONTROL_SCHEME` has two NULLDEFs in frontend.bin, so they take 0 and 1
  and its named entries start at 2.

  > **`0` is the engine's "no such def" sentinel.** An unset `DefIndex` is `0`, resolving to
  > whichever NULLDEF sits at global index 0; consumers test the resulting class index against
  > `0` to mean "unset". Emitting 1-based indices makes every unset reference resolve to the
  > *first real def of that class*. That was the local-detail bug: 260 NULLDEFs were off by
  > one, so every theme without a `LocalDetailGeneratorDef` resolved to
  > `LOCAL_DETAIL_BRIGHTWOOD_BIRCH_BRACKEN`, and the world editor scattered birch and bracken
  > over bare ground or crashed on the collision.
  >
  > **Why the divergence hunt missed it:** `verify.rs` decodes entry *bodies* only. Nothing in
  > the ledger ever compared the `NameRef` table. A systematic 260-entry defect survived a
  > "divergence closure complete" sign-off because of that blind spot. `probe_counter` /
  > `probe_counter_diff` exist to check it; a ledger arm for `NameRef` would be better.

- **Sub-def table presence is a per-wire-name property** (`def_name_has_subdef_table`,
  generated from `sub_def_names!` — 106 wire names deriving `CSubDefClassBase`/
  `CParentDefClassBase`). Present ⇒ the u16 count is always serialized, even when 0.
- **Parse is total with typed fallback**: unknown type → `DefBody::Unknown{raw}`; a known type
  whose bytes don't match propagates an error and the entry keeps raw bytes intact.
- **Two error styles coexist**: the wire layer uses composable context wrapping
  (`ParseWireError::Member{name}/Item{index}` — the good pattern); the container layer uses
  one-enum-per-function. Unification candidate.

### 4.8 Reference semantics (cross-cutting)

- **External refs are by NAME; in-body refs are global indices.** The engine looks up
  Thing→def by instantiation name, which is why own-order index assignment is safe.
- **A field's *type* declares whether it is a reference.** The ≈90 once-mistyped reference
  fields are `DefIndex` (wire = `i32` LE); plain `i32`/`u32` fields evaluate strictly, so a
  typo in a numeric field errors instead of silently becoming a def index. Corpus-measured:
  94% of integer fields are purely numeric, ~5.5% purely references, ~0 genuinely both.

---

## 5. Diagnostics

**Two tiers, one policy each. No configurable failure policies.**

- **Parse errors: strict, one per file, fail-fast, no recovery.** The first error aborts that
  file (its defs drop), is rendered, and the build moves on. A file that fails to parse **must
  always be surfaced** — its defs silently vanish otherwise.
- **Compile diagnostics: collected, many per def.** **No fallback default def** — a def with
  ≥1 error is not emitted and the build exits non-zero after reporting everything.

Everything is **collected, not rendered**: `BuildReport::diagnostics` / `BuildError::diagnostics`
carry spans, and `BuildReport::sources` carries the text they point into. `defc` and
`def-compiler-sys` each render; the library never prints.

### The diagnostic model

```rust
struct BuildDiagnostic { severity, message, labels: Vec<DiagnosticLabel>, notes: Vec<String> }
struct DiagnosticLabel { source: usize, primary: bool, span: Span, message: Option<String> }
```

- **Labels carry their own file**, so one diagnostic can point into several. Required for
  correctness, not just convenience — see §4.1 on spans outliving their file.
- **Notes carry explanation**, keeping the headline scannable. `unknown constant` and
  `unknown definition` are the same shape of mistake in two namespaces; a note says which was
  searched.
- **No message on a primary label when it would repeat the headline.** The caret carries the
  location; the headline carries the words.
- **Secondary labels are for context worth visiting.** "in this definition" is attached only
  when the offending statement is in that definition's *own* body (an exact test —
  `Span::contains` compares the file). A definition that merely inherits a broken template
  contains no trace of the problem, so labelling it wastes the reader's trip.

### Merging

`Diagnostics::take()` merges repeats of one mistake and orders everything by (file, offset).

Specialization fan-out is why: `flatten_specialization` copies a template's statements into
every descendant, so one bad statement is lowered and reported once per descendant, every
report pointing at the same byte of the same template. `OBJECT_BASE` has 3,147 transitive
descendants. Merged output is one diagnostic plus a count:

```
error: unknown constant `TEMPLATE_TYPO_XYZ`
    ┌─ objects.tpl:520:16
520 │      Health              TEMPLATE_TYPO_XYZ;
    │                          ^^^^^^^^^^^^^^^^^
    = constants come from `enum` / `#define` declarations
    = 1315 other definition(s) inherit this and fail for the same reason
```

**Genuinely distinct mistakes are never merged** — two typos of the same name on two lines have
different primary spans and both are reported in full.

### Parse-error context

`ParseError` carries a `Span` (so carets cover the offending token) and an optional
`ParseContext { what, name, span }` naming the construct the error happened *inside*. Attached
as productions unwind, innermost-first — the innermost production is the first to run on the
way out, and outer frames never overwrite it.

```
error: expected `=`, `,` or `}`, found identifier
     ┌─ Defs/gamesnds.h:1890:21
   8 │ enum    SND
     │         --- in enum `SND`
     ·
1890 │     SND_TROLL_LAUGH_01 REVERB = 1881,
     │                        ^^^^^^ expected `=`, `,` or `}`, found identifier
```

**Expected-token sets must be accurate.** `UnexpectedToken { expected, found }` builds the
"found …" half once in `Display`, so no site formats its own. The `expected` half must list
*every* token valid at that byte — an enum variant with no value yet can still take `=`, so
the set is conditional on how far the variant got. A confidently wrong expectation is worse
than a vague one; where the parser can't cheaply prove the full set, prefer a plain message
with a good span.

### The canonical corpus must compile silently

**0 warnings, 0 errors on the stock Anniversary corpus.** This is a hard invariant. The corpus
is the ground state; a build that is never quiet trains everyone to ignore the output.

Consequence: anything the stock corpus does — text outside definitions, ~800 duplicate
definition names, cross-type specialization leftovers — cannot be a warning as things stand.
`SourceAst.ignored` records the spans anyway (free, since the parser skips those tokens
regardless), waiting on a verbose mode that presents them as **notes** (§9).

---

## 6. Verification & the bug-fixing loop

Three layers, each proving something different:

| Layer | Proves | Does NOT prove |
|---|---|---|
| **`defc/tests/golden.rs`** (FNV hash of all 4 outputs; needs `OA_TEXT_DIR`; re-bless `OA_BLESS=1`) | Build is deterministic and byte-identical to the *blessed* build — **the regression gate for every refactor** | That the blessed build is correct vs retail (it can bake in bugs) |
| **`def-compiler/examples/verify.rs`** — decodes both sides to reference-resolved `SemVal`, classifies each entry `Reproduced`/`AcceptSort`/`OpaqueOnly`/`Bug`/`Missing` | **The divergence-from-retail oracle.** Where real bugs surface | Anything about *modified* defs — only in-game does |
| **In-game load** | The only proof that a *modified* or *fixed* def works | — |

`SemVal` decodes bodies through reflection, resolving `DefIndex→name` and `DefString→string`
per side, so two independent index spaces compare by *meaning*. `DiffPolicy::unordered()`
compares containers as multisets to separate MSVC sort-tie-break noise from real diffs.
Anonymous sub-defs are matched by **(parent instance name, tag)** — the one identity that
survives across index spaces.

> **The ledger is the primary bug-finding oracle, not noise.** It was once described as
> "informational only — `Bug`s are mostly noise". That was wrong and hid two large systematic
> default-value bugs (UI `Font` defaulting to null instead of `ENG_ARIAL_16`; THING
> `PersistenceFlags` defaulting to 0 instead of `EPF_STATIC`) affecting ~4,900 entries, masked
> as "index noise". Golden cannot see these because it compares to our own baseline.

### Sources of authority (when inputs disagree)

Ranked; higher wins.

1. **The retail binary itself** (`$REF`), read back through our own parser. Parsing is
   **positional** (each field control is `crc32(name)` + value in declaration order,
   crc-validated), so loading retail under our schema is a **structural oracle**: a
   `WrongId { expected, found }` pinpoints a field count/order/name mismatch at the exact byte.
2. **Retail NULLDEF entries = authoritative for field DEFAULTS.** A NULLDEF body is the
   default-constructed state serialized with no text applied.
3. **The decomp `Transfer<T>` functions** — authoritative for wire types and field order.
   `Transfer<long>` ⇒ `i32`, `Transfer<float>` ⇒ `f32`, `Transfer<CDefString>` ⇒ `DefString`.
   The order of `Transfer<…>` calls *is* the binary field order.
4. **Text source defs** — useful for what a field *means*, **not** for layout. A field the text
   writes as a string can legitimately carry `0` = null string, so "string-typed" and
   "sometimes 0" are compatible.

**NOT authoritative:** `defs-spec.json` (an early extraction that *produced* several
mistypings — it wrongly said `Font`=long, `PersistenceFlags`=defindex); the C++ copy-operator /
`TransferIn` functions (noisy — `TemplePrayerFactorHighest` is declared **twice** on purpose
because retail's `CScriptDef::Transfer` emits two consecutive controls with that crc; "fixing"
the duplicate broke positional parse of retail's script.bin).

### The bug-fixing loop (the "onion")

The ledger reports one representative field per entry, so fixing the first-diverging field
**reveals the next one underneath**. Expect to peel.

1. **Find** — run the ledger; read the `BUG diff paths` histogram and samples.
2. **Classify** — *value divergence* (real bug) vs *index-space noise* (a reference field
   showing different raw indices that resolve to the same name; only fixable by re-typing to
   `DefIndex`). `probe_field --vs $REF` compares by resolved meaning.
3. **Diagnose** — establish the correct type/default from the authority ranking.
4. **Fix** in the schema (`#[def(..., default = …)]`; change the field type; bespoke-lower
   seeding when a default needs interning — see `lower_ui`'s `ENG_ARIAL_16`).
5. **Verify** — re-run; the fixed field drops to 0; confirm Bug monotonically down.

### Byte-identical vs byte-changing fixes

- A **type fix that doesn't change encoding** (`DefIndex`↔`i32`↔`DefFlags` — all 4-byte LE) is
  byte-identical: golden stays green, no re-bless. Do these freely.
- A **default or structural fix changes bytes**: golden **fails by design**. Verify against
  `$REF`, **deploy + in-game test**, then re-bless with `OA_BLESS=1`. Do **not** re-bless a
  byte-changing fix without the in-game load test.

---

## 7. The manifest

`def-compiler/src/manifest.rs` (~10.6k generated lines) is the **only retail-derived input**.
It encodes exactly two things text can't provide: **membership** (which text defs retail
shipped per binary) and **NULLDEF lists**
(which classes get NULLDEF entries, order, and the duplication quirk — `CONTROL_SCHEME` appears
twice in frontend.bin). Deriving membership from text would change the contract from
*retail-equivalent* to *superset*, raising the in-game verification burden for no gain.
**Keep it** — small, static, honest.

**There is no regeneration tool.** The `dump_manifest` example older notes pointed at lived in
the pre-extraction OpenAlbion monorepo and did not come across; the reference survived in this
document and in `manifest.rs`'s own header for some time without anyone noticing it was dead.
Regenerating means writing it again: read the three retail binaries and, for each, emit the
`NameRef` def-name list (membership) and the leading NULLDEF run (classes, in order,
duplicates preserved — `CONTROL_SCHEME` twice). Only needed if the retail reference changes,
which it does not.

---

## 8. Settled decisions

Don't relitigate:

- The typed-struct IR and single-declaration layout principle (§3).
- Proc-macro derives as the declaration mechanism. Serde rejected (no in-place mutation);
  spec-codegen rejected (the Rust declarations have absorbed byte-verified corrections
  *against* the spec).
- The `DefReader` pull model — a legitimate answer to `&mut` aliasing, verified against every
  entry. Don't rewrite for elegance.
- One grammar for all file types (§4.1).
- Spans carry their file; no `Span::join` (§4.1).
- The manifest's existence (§7).
- The 16 KB chunk envelope. Output is ~1.17 MB larger than retail but content is
  byte-identical — the size gap is the TARGET, not a correctness gap.
- The canonical corpus compiles silently (§5).

Facet reflection was evaluated as the long-term home for `visit.rs` (its `Shape`/`Peek`/`Poke`
map onto the hand-rolled reflection) but deferred — it's 0.1.x/experimental.

---

## 9. Roadmap

### Track A — Incremental compilation (next major work)

**Goal:** a `redb`-backed cache so an edit-compile cycle costs time proportional to what
changed, not to the size of the corpus.

#### What the measurements say

From §1: **lowering + emitting game.bin is 88% of a 3.5 s build; parsing all 177 files is
10%.** So:

- **Cache lowered definitions, not parsed files.** A file-hash layer that skips *parsing* buys
  at most 10%, and only when no file changed. Worth having eventually as a cheap second layer,
  but it is not where the time is. **Def-centric is the right instinct.**
- Within game.bin, 33,525 sub-defs are lowered against 9,264 named defs. Sub-def lowering is
  likely the larger half; measure before optimising.

#### The invalidation graph

Measured on the corpus:

| | |
|---|---|
| Definitions | 10,454 (7,172 have a parent) |
| Chain depth | max 8, median 1 |
| **Leaf definitions (0 descendants)** | **9,775 (93.5%)** |
| Definitions with ≥100 transitive descendants | 23 |
| Largest | `OBJECT_BASE` 3,147 · `OBJECT_VILLAGE_SOLID_FURNITURE_BASE_TEMPLATE` 1,716 · `CREATURE_BASE_TEMPLATE` 523 |

The distribution is bimodal and favourable: **93.5% of definitions are leaves**, so the typical
modder edit invalidates exactly one def. A root-template edit invalidates up to 30% of the
corpus, which is a correctness requirement, not a performance problem — it just has to be
*correct*, never under-invalidating.

Cascade rule: editing a def invalidates itself and **all transitive descendants**. Tagged
blocks merge across the chain too, so sub-def outputs cascade identically.

#### The hard part: cached bodies are not position-independent

This is the thing that will sink a naive implementation. A lowered `DefBody` contains two kinds
of corpus-global state:

1. **`DefIndex` = a global entry index**, assigned by position in `collect_named`, which
   depends on the whole corpus and on file order. Adding or removing *any* earlier def shifts
   every later index, invalidating every cached body that references one.
2. **`DefString` = a byte offset into the shared `names.bin`**, which depends on interning
   order across the entire build.

So a cached body must keep references **symbolic** (by name / by string) and resolve them at
link time. The pipeline once had exactly this seam — a `LowerEnv` trait with `def_index(name)`
+ `def_string_offset(str)` — and **it no longer exists**: `lower_def` now takes
`def_indices: &HashMap<String,u32>` and `names: &RefCell<NamesBuilder>` directly.
**Restoring that seam is a prerequisite**, not an optimisation.

That gives the compile/link split: *compile* a def to a body with symbolic references (cacheable,
position-independent), then *link* — assign indices, intern strings, patch, serialize.

#### What to hash

Hash the **statement list after flattening is excluded** — i.e. the def's own body — plus its
type and parent name. Then cascade. Options for the hash input:

- **Text source of the def.** Simple and obviously correct, but trivia-sensitive: reformatting
  or a comment edit invalidates. Given the corpus is CRLF and heavily commented, this will
  cause spurious rebuilds.
- **The AST.** Skips trivia by construction. Requires a stable, span-*excluding* hash — spans
  change when anything above them in the file moves, so hashing them defeats the purpose.
  `Spanned<T>: PartialEq` already ignores spans, so the precedent exists; a `Hash` impl must do
  the same.

Recommend the AST hash, with the span-exclusion property covered by a test that inserts a
comment and asserts the hash is unchanged.

Correctness obligations, in priority order — **under-invalidation is a silent wrong-output
bug, over-invalidation only costs time**:

- The symbol table is global and two-pass (all headers + all `.def`-local declarations are
  evaluated before any lowering). Any symbol change potentially affects any def that reads it.
  Either track per-def symbol reads or treat a symbol-table change as a full invalidation to
  start with.
- Duplicate definition names: first occurrence wins the index, last wins the body (§4.6). The
  cache key must be the *resolved* def, not the file-local one.
- The manifest, the schema (`defs` crate), and the compiler version all affect output. Bake a
  version stamp into the cache key so a code change can't serve stale bodies.

#### Suggested sequence

1. Restore the `LowerEnv` seam; make lowered bodies reference-symbolic. Golden-gated, no cache
   yet, no behaviour change.
2. Measure named vs sub-def lowering split to confirm where the 3.09 s goes.
3. Build the specialization graph explicitly (it is currently rediscovered by walking
   `specializes` on demand) and use it for both flattening and cascade.
4. Add the AST hash + `redb` store, keyed on def name, valued by (hash, symbolic body).
5. Only then consider the file-hash layer to skip parsing.

Free win available immediately: `flatten_specialization` runs **twice per def** (§4.6).

### Track B — Diagnostics, remaining

- **Verbose mode** presenting `SourceAst.ignored`, cross-file duplicate definition names
  (786 in the corpus vs 18 within a single file — the latter are near-certainly bugs), and
  other corpus oddities as **notes**, so the default build stays silent (§5).
- **Unconsumed-statement warnings.** Removed, not deferred. `remaining_statements()` answered
  "did *this reader* consume it?", but consumption is spread across several readers and
  pipeline stages: tagged blocks are consumed by the sub-def pass, `Components.Add` by
  `build_thing_components`, and the bespoke UI/CONTROL_SCHEME arms run a second full-body
  reader over statements the generic pass already took. Measured against the stock corpus it
  produced **103,467 "unconsumed" statements, of which 10 were genuine**. Reviving it needs a
  **build-scoped consumption ledger keyed on statement identity** and checked once at the end —
  not a `Vec` returned per reader. `Span` now carrying a `FileId` makes that identity possible.
- **Bespoke-arm error propagation** — ~39 `.ok()`/`unwrap_or` sites (§4.5). Highest-risk first:
  `build_animation_anims` (a typo'd animation key silently gives bank 0), then the `OPINION_*`
  ctor closures. Warn before promoting to error, and gate each step on golden *and* on the
  corpus staying silent.
- **"Did you mean?"** on `unknown constant` / `unknown definition` / unknown field / unknown
  parent. Bounded edit distance over the candidate set, which each site already has in hand.

### Track C — Quality

- **Unit tests for `lower.rs`** — 3,350 lines, **zero** tests. The bespoke arms (ctor arg
  remapping, DSL replay, sort/dedup fixups) rely entirely on the golden end-to-end gate.
  Prerequisite for Track B's error-propagation work.
- **A `NameRef` arm for the semantic ledger** — the blind spot that hid the 260-entry
  ClassIndex bug (§4.7).
- **Enum typing** — a few fields remain `i32`/`u32` where a `DefEnum`/`DefFlags` exists. All
  byte-identical. `PersistenceFlags` for the Thing family needs decomp extraction first.
- **Schema-ize bespoke lowering** — `#[def(container = …)]`, declarative ctor-arg maps, to move
  §4.5's B and D categories out of code.
- **Container-layer error unification** — adopt the wire layer's composable context wrapping
  (§4.7).

---

## 10. Operational reference

### Commands

```bash
REF=~/Fable/data/CompiledDefs/backup-retail-verified
TEXT=~/doc/Fable_Anniversary-2013-02-25/Fable/Data

# From-scratch build (all four binaries)
cargo run -q -p defc -- $TEXT/Defs <out_dir>

# Golden byte-determinism gate (THE regression gate)
OA_TEXT_DIR=$TEXT/Defs cargo test -p defc --test golden
# ...re-bless after an INTENDED output change (see §6 first):
OA_BLESS=1 OA_TEXT_DIR=$TEXT/Defs cargo test -p defc --test golden

# Semantic ledger vs retail (build first, then verify the output)
cargo run -q -p def-compiler --example verify -- <out_dir> $REF
cargo run -q -p def-compiler --example verify -- <out_dir> $REF --dump-subdef <Parent> [<Tag>]

# Probe/compare a single field vs retail (the divergence workhorse)
cargo run -q -p def-compiler --example probe_field -- <dir> <game|frontend|script> <Field> --vs $REF
cargo run -q -p def-compiler --example probe_field -- <dir> game --entry NULLDEF_UI --vs $REF

# NameRef / ClassIndex checks (the ledger's blind spot)
cargo run -q -p def-compiler --example probe_counter -- <dir>
cargo run -q -p def-compiler --example probe_counter_diff -- <dir> $REF

# NOTE: there is no manifest-regeneration tool in this repo. The `dump_manifest`
# example older notes referred to lived in the pre-extraction OpenAlbion monorepo
# and did not come across. See §7.

# Phase timing (§1) — release build, timestamped progress lines
cargo build -q --release -p defc
./target/release/defc $TEXT/Defs /tmp/out 2>&1 \
  | while IFS= read -r l; do printf '%s\t%s\n' "$(date +%s.%3N)" "$l"; done \
  | awk -F'\t' 'NR==1{t0=$1} /compiling/{tc=$1} /game:/{tg=$1} /script:/{ts=$1} END{
      printf "parse %.2fs  game %.2fs  fe+script %.2fs\n", tc-t0, tg-tc, ts-tg }'

# In-game test — deploy all four; ROLLBACK from $REF if it won't load
cp <out_dir>/{game,frontend,script,names}.bin ~/Fable/data/CompiledDefs/
cp $REF/{game,frontend,script,names}.bin ~/Fable/data/CompiledDefs/   # rollback
```

### Key files

| File | Role |
|---|---|
| `packages/defs/src/text/mod.rs` | **The grammar**: `Item`, `SourceAst`, AST types, all productions |
| `packages/defs/src/text/lexer.rs` | `lex`/`Cursor`/`TokenKind`/`TextParseErrorKind` |
| `packages/defs/src/text/base.rs` | `FileId`, `Span`, `Spanned`, `ParseContext`, `ParseError` |
| `packages/defs/src/text/symbols.rs` | `SymbolTable`, `SymbolEvalError`, `Redefinition` |
| `packages/defs/src/binary.rs` | Container + generated `DefBody`/`sub_def_names!` + `SemVal` |
| `packages/defs/src/wire.rs` | Wire model + runtime helpers |
| `packages/defs/src/enums.rs` | `DefEnum`/`DefFlags` + all enum tables |
| `packages/defs/src/visit.rs` | `FieldRef` + slot traits (reflection) |
| `packages/defs/src/def/*.rs` | ~273 def struct declarations, one module per type |
| `packages/defs-derive/src/lib.rs` | The five derives + `#[def]`/`#[flags]` attr parsing |
| `packages/def-compiler/src/reader.rs` | `Evaluator`/`Args`/`DefReader` |
| `packages/def-compiler/src/lower.rs` | `flatten_specialization`; `Applier`; ~35 bespoke arms; `lower_def`/`lower_generic` |
| `packages/def-compiler/src/build.rs` | Corpus parsing, assembly, **all diagnostic construction** |
| `packages/def-compiler/src/manifest.rs` | Generated retail membership + NULLDEF lists |
| `packages/defc/src/main.rs` | CLI + diagnostic rendering |
| `packages/defc/tests/golden.rs` | **Byte-determinism gate** |
| `packages/def-compiler/examples/verify.rs` | Semantic ledger — the divergence oracle |

Test counts: `defs` 129, `def-compiler` 41, `defc` 1 (golden), plus doctests.

### Load-bearing gotchas

- **CRC32** = standard zlib (poly `0xEDB88320`, init 0, no final xor) = `CCharString::GetCRC`.
- **`EntryPreamble.unknown_0 = 1`** for ALL non-NULLDEF entries (`AreDefaultValsApplied`;
  `0` ⇒ save-load crash). NULLDEF preambles are `(false, false, 0)`.
- **`NameRef.ClassIndex` is 0-based and continuous across all three emission passes** (§4.7).
- **Tagged blocks MERGE** across specialization (parent-first concat), never replace.
- **Anonymous entry `file_name_offset` = `0xFFFFFFFF`**; dedup keys on **(class-tag, bytes)**
  (byte-only type-confuses combat sub-defs → crash).
- **`names.bin` is ONE shared table** across all three; header off8/off12 content-derived.
- **External def refs are by NAME; in-body refs are global indices.**
- **`DefString::def_default()` = −1; `DefIndex::def_default()` = 0.**
- **Text-tag fields mis-typed as `DefIndex`** (`CShopDef.Name`, `CTavernGameDef.Banter`…) hold
  hashed text ids — "out of range" but load-safe.
- **The chunk-index sentinel entry is required**; chunks compress at zlib level 1.
- **CONTROL_SCHEME appears twice** in frontend.bin NULLDEFs — iterate `NULLDEF_ENTRIES`
  (non-deduped) for output, build bodies from `NULLDEF_CLASSES` (deduped).
- **`_deprecated.` files are skipped** by the corpus walk.

### Decomp reference map

| What | Where |
|---|---|
| CRC32 | `bbblibrary/lib_crc.cpp` |
| Def / sub-def compile | `fablelib/defs/definition_manager.cpp`, `bbblibrary/lib_definition_manager.cpp` (`CompileSubDefinition`) |
| Def lookup | `lib_definition_manager.hpp` (`GetPDefFrom{GlobalIndex,InstantiationName}`) |
| `ClassIndex` write / read | `lib_definition_manager.cpp:1581` / `GetDefClassIndexFromGlobalIndex` |
| names.bin format | `bbblibrary/lib_definition_string.cpp` (`CDefStringTable`) |
| Thing def ref = name | `fablelib/thing.cpp`; anon entry sentinel `thing_base_def.cpp` |
| Animation / opinion / sound / controls | `animation_set.cpp`; `tc_opinion_of_hero.cpp`, `opinion_reaction_manager.cpp`; `lib_sound_map.cpp`; `fablelib/defs/controls_def.cpp` |
| Thing components | `CThingComponentSet::Add` (driver types via `GetPTCInfo`) |
| Script def inheritance | `fablescripting/defs/{script_def.hpp:258,cutscene_def.hpp,regionscriptdef.hpp}` |
| **Wire types & field order (AUTHORITATIVE)** | `<Class>::Transfer` in `~/git/fable-decomp/out/source/**/*.cpp` |
| Layout spec (NOT authoritative) | `fable-decomp/defs-spec.json` — superseded wherever it disagrees |
