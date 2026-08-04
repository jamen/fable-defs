# Incremental Compilation: Research & Analysis

## Summary

The full build takes **3.5 s** on the Anniversary corpus (177 files, 10,454 definitions,
13,239 game.bin entries). Lowering + emitting game.bin is 88% (3.09 s). Parsing all files is
10% (0.35 s).

The distribution is favourable: 93.5% of definitions are leaves (0 transitive descendants).
Only 23 defs have ≥100 descendants; the largest (`OBJECT_BASE`) has 3,147.

**Goal:** a `redb`-backed cache so an edit-compile cycle costs time proportional to what
changed.

---

## 1. The problem: lowered bodies are not position-independent

A lowered `DefBody` carries two kinds of corpus-global state:

### 1.1 `DefIndex` = global entry index

A `DefIndex` field like `DummyObject` resolves through `def_indices: &HashMap<String, u32>`
at lowering time. The map is built by `collect_named`, which assigns indices based on sorted
file order. Adding or removing any earlier entry shifts every later index — invalidating every
cached body.

**Scale:** 221 DefIndex usage sites across all def structs: 195 scalar, 22 `Vec<DefIndex>`,
3 `BTreeMap`/`VecMap<K, DefIndex>`, 1 inside a WireStruct. 88% are scalar.

### 1.2 `DefString` = names.bin byte offset

A `DefString` field like `Font` resolves through `names.borrow_mut().intern(s)` at lowering
time. The byte offset depends on interning order across the entire build — adding or removing
any string before this one shifts all later offsets.

**Scale:** 204 DefString usage sites: 156 scalar, 21 `Vec<DefString>`, 7 `VecMap<K, DefString>`,
1 `VecMap<DefString, i32>`, 13 inside WireStructs, 4 inside DefVariants, 2 `Vec<DefString>`
inside WireStructs.

### 1.3 Conclusion

A cached lowered body must keep references **symbolic** (by name / by string) and resolve them
at link time when indices and interning order are known. The mechanism for this is a
**relocation table**.

---

## 2. Design: relocation table

### 2.1 Core concept

During lowering, whenever a `DefIndex` or `DefString` field is set, record a relocation entry:
`(wire_field_crc, symbolic_value)`. The DefBody stores the RESOLVED values (as it does today);
the relocation table stores what name/string produced each value.

At link time: walk the DefBody, look up each relocation entry by field CRC, resolve the
symbolic name/string through the NEW `def_indices`/`NamesBuilder`, and overwrite the field.

### 2.2 Why not `LowerEnv`

`LowerEnv` was a trait abstraction over the two concrete arguments (`def_indices` and `names`).
It was **intentionally removed** — the two concrete arguments are simpler and equivalent. The
relocation table is orthogonal to the lowering interface: it records what a field was set *to*,
not what mechanism performed the resolution. No trait is needed; we just need to record at the
points where resolution happens.

### 2.3 Recording strategy

Pass a `&mut RelocRecorder` through the lowering pipeline as an additional parameter alongside
`def_indices` and `names`. At each point where a `DefIndex`/`DefString` value is resolved,
also record the symbolic value.

**`RelocRecorder` is a plain struct, not a trait:**

```rust
#[derive(Default)]
pub struct RelocRecorder {
    /// (crc32(wire_name), symbolic_def_name) — one entry per DefIndex field set.
    /// For container elements, entries appear in the order the elements are stored.
    def_refs: Vec<(u32, String)>,
    /// (crc32(wire_name), symbolic_string) — one per DefString field set.
    def_strings: Vec<(u32, String)>,
}
```

### 2.4 Recording in the generic path (Applier)

There are exactly two sites in the generic lowering path:

| Location | Code | Recording |
|---|---|---|
| `apply_expr` line 918 | `*slot = DefString(self.eval_def_string(expr)?)` | Record `(crc(wire_name), string_value)` |
| `apply_expr` line 920 | `*slot = DefIndex(resolve_ref_i32(...)?)` | Record `(crc(wire_name), def_name)` |
| `apply_expr` line 909 | `Enum` via `resolve_ref_i32` | Record `(crc(wire_name), def_name)` if resolved via def_indices |
| `apply_expr` line 915 | `Flags` via `resolve_ref_i32` | Same as Enum |

But `apply_expr` doesn't have the wire name — it receives a `FieldRef`. The wire name is
available in `apply_named` (the `name: &'static str` parameter). We'd pass it through.

**Better approach:** record at the `resolve_ref` / `resolve_ref_i32` / `eval_def_string`
level, where the symbolic value is directly available from the expression being evaluated:

- `resolve_ref_i32(expr)` → when a def name is found (string literal or unknown symbol),
  extract the name from `expr` and record it.
- `eval_def_string(expr)` → when a string literal is interned, extract the string from
  `expr` and record it.

But these functions don't know which FIELD they're resolving for. The field name (wire name)
needs to come from the caller.

**Cleanest approach: record in `apply_expr`, threading the wire name:**

```rust
// In apply_named (and all callers of apply_expr that know the name):
fn apply_named(&mut self, name: &'static str, field: FieldRef<'_>) -> ... {
    ...
    if let Some(expr) = self.r.opt_expr(name) {
        self.apply_expr(name, field, expr)?;  // pass name through
    }
    ...
}
```

For container elements (`apply_from_group`, `apply_from_args`), the caller knows the parent
field's wire name. We propagate that to `apply_expr`.

### 2.5 Recording in bespoke arms

The bespoke arms that resolve DefIndex/DefString directly:

| Function | Line | What it resolves |
|---|---|---|
| `lower_ui` | 498 | `names.intern("ENG_ARIAL_16")` — hardcoded string |
| `lower_ui` | 532, 538, 544, 560, 601, 648 | `resolve_ref` — 6 sites |
| `lower_ui` | 552 | `names.intern(s)` — 1 site |
| `apply_ui_state` | 673 | `def_indices.get(name)` — 1 site |
| `apply_ui_state` | 678 | `resolve_ref` — 1 site |
| `build_thing_components` | 2199 | `names.intern(n)` — 1 site per component |
| `build_animation_anims` | 1767, 1781, 1931 | `names.intern` — 3 sites |
| `anim_arg_defstring` | 1738 | `names.intern` — helper, called from build_animation_anims |
| `lower_entity_sound_def` | 2858 | `names.intern` — 1 site |
| `lower_flammable_def` | 2897 | `names.intern` — 1 site |
| `try_apply_object_augmentation_particle_set` | 1307 | `names.intern` — called for string args |
| `lower_hero_morph_def` | 2529, 2544, 2560, 2575 | `anim_arg_defstring` — 4 sites |
| `build_nulldefs` (build.rs:1091) | — | passes through to lower_def |
| `emit_nulldef_and_named` (build.rs:1170-1171) | 1170, 1171 | `names.intern` for NameRef fields |
| `build_subdefs` (build.rs:1404) | 1404 | `names.intern` for sub-def tags |

These sites receive `reloc: &mut RelocRecorder` and record directly. Each site already has the
wire name (the `#[def("WireName")]` name for the field being resolved).

### 2.6 Container elements

For `Vec<DefIndex>`, `BTreeMap<K, DefIndex>`, etc., the relocation table records entries in
the same order the elements are stored. At link time, the patcher iterates the container in
the same order and resolves each relocation entry.

Example: `UI.Children` is `Vec<DefIndex>`. During lowering, `lower_ui` builds the vec with
`set_grow(&mut out.children, idx, DefIndex(value), DefIndex(0))`. The relocation table records
entries as: `(crc("Children"), "OBJECT_X")`, `(crc("Children"), "OBJECT_Y")`, etc., in the
order they were added.

At link time, the patcher walks `Vec<DefIndex>`, resolves entry 0 → `def_indices["OBJECT_X"]`,
entry 1 → `def_indices["OBJECT_Y"]`, and sets them.

**Key invariant:** the number and order of relocation entries for a container field EXACTLY
match the number and order of elements in that container. This holds as long as:

1. Each `Add` / indexed assignment records its relocation entry.
2. `clear()` resets both the container and the relocation entries for that wire name.
3. Container elements are never added by the generic lowering for a field that a bespoke arm
   also handles (the two paths are mutually exclusive — a field either goes through bespoke
   or generic, never both).

**This holds.** The generic path's `apply_vec_named` and `apply_map_named` are the sole
producers of container elements in the generic path. The bespoke arms that handle containers
(`lower_ui`, `build_animation_anims`, `build_thing_components`, etc.) build the containers
explicitly and record relocations alongside.

### 2.7 The CachedDef type

```rust
struct CachedDef {
    ast_hash: u64,
    def_type: String,
    body_bytes: Vec<u8>,      // serialized DefBody with resolved but possibly-stale values
    def_refs: Vec<(u32, String)>,    // (crc32(wire_name), symbolic_def_name)
    def_strings: Vec<(u32, String)>, // (crc32(wire_name), symbolic_string)
}
```

At link time:
1. Deserialize `body_bytes` → `DefBody`.
2. Walk via `visit_fields`, applying relocations: for each `(crc, symbolic_name)` entry at
   a DefIndex field, resolve `symbolic_name` → new index via NEW `def_indices`, set.
   For each `(crc, symbolic_string)` at a DefString field, intern `symbolic_string` via
   NEW `NamesBuilder`, set.
3. Re-serialize.
4. Container elements consume relocation entries in order (consume from the front of the
   per-wire_name list).

---

## 3. AST hashing

### 3.1 What to hash

The lowering input is: **(type name, parent name, flattened statement list)** plus the
symbol table. Since the symbol table is global, any change to it invalidates every cached def
(conservative but correct; headers rarely change in practice).

Hash the def's **own** body (before flattening), plus its type name and parent name. The
flattened body is derived from the own body + parent chain, so hashing the own body +
parent name covers it.

### 3.2 Span exclusion

`Spanned<T>: PartialEq` already ignores spans. We need `Hash` to do the same:

```rust
impl<T: Hash> Hash for Spanned<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}
```

Then derive `Hash` on all AST types (`Definition`, `Statement`, `Field`, `MethodCall`,
`TaggedBlock`, `Call`, `PropertyPath`, `PathSegment`, `Expr`). Test: insert a comment/blank
line → hash unchanged.

### 3.3 What goes in the key

```
Cache key: concat("def:", def_name)
Cache value: CachedDef { ast_hash, def_type, body_bytes, def_refs, def_strings }
```

A `schema_version: u32` and `symbol_table_hash: u64` are database-level metadata. On schema
change or symbol table change, the entire cache is invalidated.

---

## 4. Specialization graph

### 4.1 Current state

`flatten_specialization` (lower.rs:158) walks the `specialises` chain on demand, called
**twice per def** — once in `emit_nulldef_and_named` for the named body, once in
`build_subdefs` for sub-def extraction.

### 4.2 Fix: compute once

Build an explicit `SpecGraph` after parsing:

```rust
struct SpecGraph {
    /// def name → immediate children (reverse edges for cascade)
    children: HashMap<String, Vec<String>>,
    /// def name → chain of ancestor names (most-distant first), including self
    chains: HashMap<String, Vec<String>>,
}
```

`flatten_specialization` uses the precomputed chain. Cascade invalidation uses the reverse
edges: editing a def invalidates itself + all transitive descendants.

### 4.3 Cache the flattened body too

The flattened statement list is the actual input to lowering. Caching it avoids re-computing
the specialization chain. The `CachedDef` can optionally include the flattened body for faster
re-lowering when the cache misses but the parent chain is unchanged.

---

## 5. redb database

### 5.1 Schema

```
"schema_version"      → u32
"symbol_table_hash"   → u64
"def:<name>"          → CachedDef (bincode-serialized)
```

### 5.2 Database location

`<output_dir>/.defcache/` — per-corpus, so different input directories have separate caches.

### 5.3 Build flow with cache

```
For each binary (game, frontend, script):
  1. Scope files, build defs_by_name, assign indices (collect_named).
  2. For each named def (in index order):
     a. Compute AST hash (own body + type + parent).
     b. Look up cache key "def:<name>".
     c. If hit and ast_hash matches and symbol_table_hash matches:
        - Deserialize CachedDef.
        - Link: patch body_bytes using relocations + new def_indices/names.
        - Re-serialize.
        - Emit.
     d. If miss:
        - flatten_specialization (from SpecGraph).
        - lower_def with RelocRecorder.
        - Serialize body_bytes.
        - Store CachedDef in cache (write-through or batch at end).
        - Emit.
  3. Build sub-defs (game.bin only) — always from scratch for now.
  4. Assemble and write binary.
```

---

## 6. Implementation sequence

### Step 0: SpecGraph + flatten_specialization dedup
- Build `SpecGraph` once in `parse_corpus`.
- `flatten_specialization` uses the precomputed chain.
- Save flattened body so `build_subdefs` doesn't re-compute.
- Measure: reduces duplicate work, simple and safe.

### Step 1: Add `Hash for Spanned<T>` and derive Hash on AST types
- Add the impl in `defs/src/text/base.rs`.
- Derive `Hash` on `Definition`, `Statement`, `Field`, `MethodCall`, `TaggedBlock`, `Call`,
  `PropertyPath`, `PathSegment`, `Expr`, `Item`, `EnumDecl`, `EnumVariant`, `EnumExpr`,
  `Define`, `Namespace`, `IfDef`.
- Test with a file that adds a comment → hash unchanged.
- Also add a hash for `SourceAst` (all definitions, their own bodies).

### Step 2: Add RelocRecorder and thread it through lowering
- Define `RelocRecorder` in a new `packages/def-compiler/src/reloc.rs`.
- Thread `&mut RelocRecorder` through `lower_def`, `lower_generic`, `Applier`, and each
  bespoke arm.
- At each resolution point, record the symbolic value alongside the resolved value.
- Validate: build with recording enabled, walk the result to verify relocation entries match
  the DefBody's resolved values. Golden must stay green (recording doesn't change output).

### Step 3: Implement link-time patching
- `fn patch_body(body: &mut DefBody, relocations: &[(u32, String)], def_indices: &HashMap<String, u32>, names: &RefCell<NamesBuilder>)`.
- Walks via `visit_fields`, matches relocation entries by wire name CRC, resolves, sets.
- Container element handling: consume entries in order.
- Validate: round-trip test — lower with recording, serialize, deserialize, patch with same
  def_indices/names, serialize again. Output must be byte-identical.

### Step 4: Add redb cache
- Add `redb` dependency to `def-compiler`.
- `CacheDb` wrapper: open/create at `<output>/.defcache/`.
- Cache lookup by ast_hash + symbol_table_hash.
- Write-through on miss.
- Symlink or copy from previous output's cache on first build.

### Step 5: Cascade invalidation
- Use `SpecGraph` to invalidate all transitive descendants when a def's AST hash changes.
- Initially: any symbol table change invalidates all entries.

### Step 6 (optional): File-hash layer
- Cache parsed `SourceAst` per file. Only 10% of build time, low priority.

---

## 7. What's NOT in scope (initially)

- **Sub-def caching**: tagged blocks are merged, lowered, deduped. The `(tag, bytes)` dedup
  makes caching complex. For the MVP, sub-defs are always re-lowered from scratch. If profiling
  shows them as dominant, cache them as part of the parent's `CachedDef` entry.

- **Per-symbol invalidation**: any symbol table change invalidates everything. Refine later
  by tracking per-def symbol reads.

- **File-hash layer**: skip parsing unchanged files. Low ROI (10% of build time).

- **Build assembly refactoring**: `NamesBuilder` interning for NameRef fields
  (def_name, file_name) is not position-dependent for the cache — these are always interned at
  link time and don't need relocation entries.

---

## 8. Key files to create/modify

| File | Action |
|---|---|
| `packages/def-compiler/src/reloc.rs` | New: `RelocRecorder`, `CachedDef`, `patch_body` |
| `packages/def-compiler/src/cache.rs` | New: redb I/O, cache lookup/write |
| `packages/def-compiler/src/graph.rs` | New: `SpecGraph` |
| `packages/def-compiler/src/lower.rs` | Thread `&mut RelocRecorder` through all lowering arms |
| `packages/def-compiler/src/build.rs` | Integrate SpecGraph, cache lookup, link phase |
| `packages/defs/src/text/base.rs` | `impl Hash for Spanned<T>` |
| `packages/defs/src/text/mod.rs` | Derive `Hash` on AST types |
| `packages/def-compiler/Cargo.toml` | Add `redb`, `bincode` (or `postcard`) |

---

## 9. Risk assessment

### Risk: Missing a relocation recording site
If a DefIndex/DefString is resolved somewhere that doesn't record into `RelocRecorder`, the
relocation table won't have an entry for that field. At link time, the stale value is kept
unchanged in the DefBody.

**Mitigation:** A validation pass after lowering that walks the DefBody via reflection, checks
every `DefIndex(v ≠ 0)` field against the relocation table, and reports any unmatched values.

### Risk: Container element ordering mismatch
If a bespoke arm builds a container in a different order than the relocation entries are
recorded, the patcher will misalign.

**Mitigation:** Record relocations at the EXACT same call site where the element is added.
In the generic path, `apply_vec_named` records after `push_default` + `apply_from_group`.
In bespoke arms, record immediately after the container mutation.

### Risk: Schema version mismatch
If the def structs change (new fields, different types), cached bytes from a previous compiler
version are incompatible.

**Mitigation:** `schema_version` in the database. Bump it manually when any `#[derive(DefStruct)]`
or `DefBody` variant changes. The golden gate catches this because a schema change breaks
golden too.
