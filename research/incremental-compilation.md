# Incremental Compilation: Research & Analysis

## Summary of current state

The full from-scratch build takes **3.5 s** on the Anniversary corpus (177 files, 10,454
definitions, 13,239 game.bin entries). Lowering + emitting game.bin is 88% of that (3.09 s);
parsing all files is only 10% (0.35 s). The distribution is bimodal and cache-friendly:
93.5% of definitions are leaves (0 descendants), and only 23 definitions have ≥100 transitive
descendants. The largest, `OBJECT_BASE`, has 3,147.

**Goal:** a `redb`-backed cache so an edit-compile cycle costs time proportional to what
changed, not to the size of the corpus.

---

## 1. Current pipeline walkthrough

### Phase 1: parse_corpus (build.rs:825-908)
1. Walk `Defs/` for `.h` files → parse each into `SourceAst` → `SymbolTable::evaluate_items`
   → one global symbol table. Header-set resolution picks one of `RetailHeaders/DevHeaders`.
2. Walk `Defs/` for `.def`/`.tpl` files → parse each into `SourceAst`; definitions are
   extracted, `ParsedFile` stores the full AST.
3. Evaluate symbols from `.def`/`.tpl` files too (enums, `#define`s declared inline).
4. Produces `ParsedCorpus { files, symbols, sources, def_to_source, def_spans }`.

### Phase 2: build_one_bin (×3 — game, frontend, script)
1. **Scope** files by binary (all for game, `FrontEndDefs/` for frontend, `ScriptDefs/` for script).
2. **`defs_by_name`**: last-file-wins map of definition name → `&Definition`.
3. **`collect_named`**: assigns global indices (NULLDEF count + position). First occurrence
   claims the slot; duplicates in later files are skipped. Filtered by manifest membership.
4. **`build_nulldefs`**: `lower_def(type, None, &[], ...)` → `def_default()` for each class.
5. **`emit_nulldef_and_named`**:
   - Emit NULLDEF entries (preamble `is_real=false`).
   - For each named def: `flatten_specialization` (parent chain → flat `Vec<Statement>`) →
     `lower_def` (dispatch on type name) → `Built { body: DefBody, ... }`.
   - `NamesBuilder::intern` is called for def_name, file_name during emission; also for
     `DefString` values inside `lower_def`.
   - `next_class_index` assigns 0-based counter per class.
6. **`build_subdefs`** (game.bin only): for each named entry with a sub-def table,
   `flatten_specialization` **again**, extract tagged blocks, merge by CRC of tag, lower each,
   dedup by `(tag, bytes)`, emit anonymous entries.
7. **`assemble_and_write`**: chunk entries at 16 KB, zlib compress, serialize `DefBinary`.

### Key observation: `flatten_specialization` is called twice per def
Once in `emit_nulldef_and_named` for the named body, once in `build_subdefs` for sub-defs.
This is a known cheap win (§9 AGENTS.md).

---

## 2. The two kinds of global state in lowered bodies

A lowered `DefBody` is **not position-independent**. It carries two kinds of corpus-global
state that would be invalidated if any earlier entry in the same binary changes:

### 2.1 `DefIndex` fields — global entry indices

`DefIndex` is `i32` on the wire (4 bytes LE, default 0). At lowering time, a def reference
like `DummyObject "OBJECT_BANDIT_GRUNT"` or `Children[i] OBJECT_VILLAGE_TAVERN` is resolved
through `def_indices: &HashMap<String, u32>` — the map built by `collect_named`.

If a new named entry is added **before** a referenced def in the sorted file order, every
`DefIndex` referencing it shifts by +1. This would invalidate every cached body that
references it.

**Scale**: ~90 fields are `DefIndex` (mistyped ones already fixed). But any def that
references another def — which is most of them — would be invalidated by index shifts.

### 2.2 `DefString` fields — names.bin byte offsets

`DefString` is `i32` on the wire (default −1). At lowering time, a string like
`Font "ENG_ARIAL_16"` or an animation key is passed to `names.borrow_mut().intern(s)`, which
returns the byte offset in the shared `names.bin`. The offset depends on **interning order**
across the entire build.

If a new string is interned before an existing one, every `DefString` with that offset shifts.
This would invalidate every cached body that contains a `DefString`.

**Scale**: every lowered body that uses strings (animation names, component type names, UI
fonts, etc.) has at least one `DefString`. That's most of them.

### Conclusion

A cached lowered body must keep references **symbolic** — by name instead of by index, by
string instead of by offset. Resolution happens at **link time** when indices and interning
order are known. This is the compile/link split.

---

## 3. The missing `LowerEnv` seam — what it was and how to restore it

AGENTS.md §9 says:

> The pipeline once had exactly this seam — a `LowerEnv` trait with `def_index(name)` +
> `def_string_offset(str)` — and it no longer exists: `lower_def` now takes
> `def_indices: &HashMap<String,u32>` and `names: &RefCell<NamesBuilder>` directly.

### Current signature (lower.rs:3251)

```rust
pub fn lower_def(
    name: &str,
    base: Option<&DefBody>,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
    def_indices: &HashMap<String, u32>,   // resolves DefIndex at lower time
    names: &RefCell<NamesBuilder>,         // interns DefString at lower time
) -> Result<DefBody, LowerError>
```

And every bespoke arm, `lower_generic`, and the `Applier` struct passes `def_indices` and
`names` through.

### What the seam looked like (reconstructed from AGENTS.md description)

```rust
trait LowerEnv {
    fn def_index(&self, name: &str) -> Option<u32>;
    fn def_string_offset(&self, name: &str) -> u32;  // intern if needed
}
```

### Restoration plan

The simplest approach that avoids touching all ~35 bespoke arms (3,350 lines) is to **replace
the concrete types with a trait object**:

1. Define a `LowerEnv` trait in `def-compiler`:

```rust
pub trait LowerEnv {
    /// Resolve a definition name to a symbolic reference.
    /// Returns a SymbolicRef that represents a name → index binding.
    fn def_ref(&self, name: &str) -> Result<SymbolicRef, LowerError>;
    /// Intern a string for a CDefString field.
    /// Returns a SymbolicString that represents a string → offset binding.
    fn def_string(&self, s: &str) -> SymbolicString;
}
```

2. Introduce opaque symbolic types that replace `DefIndex`/`DefString` in cached bodies:

```rust
/// A definition reference, resolved at link time.
/// In a cached body, this carries the name string.
/// In a linked body, it carries the resolved u32 index.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SymbolicRef {
    Unresolved(String),
    Resolved(u32),
}

/// A CDefString, resolved at link time.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SymbolicString {
    Unresolved(String),
    Resolved(i32),
}
```

3. **But this means `DefBody` can't hold `SymbolicRef`/`SymbolicString` directly** — the
   `DefIndex`/`DefString` types are hard-coded in the wire model generated by proc macros.
   Changing them would mean changing every struct declaration (~273 types).

**Alternative: keep `DefBody` unchanged, store resolved values, and make cache entries carry
a separate "relocation table":**

```
CachedBody {
    body: DefBody,           // DefIndex=0, DefString=-1 placeholders
    def_refs: Vec<(usize, String)>,    // offset → name
    def_strings: Vec<(usize, String)>,  // offset → string
}
```

Where `offset` is the byte offset of each `DefIndex`/`DefString` within the serialized body.
At link time, patch these with the resolved values, serialize, done. No schema changes needed.

**This is the preferred approach.** It avoids touching any struct declarations, any proc-macro
code, and any lowering arm. The compile phase produces a `DefBody` with placeholder indices
(0 for `DefIndex`, −1 for `DefString`), plus a relocation table. The link phase patches.

### Implementation approach for the relocation table

During lowering, instead of passing `&HashMap<String, u32>` and `&RefCell<NamesBuilder>`,
pass wrappers that:
- For `DefIndex`: record the name → offset mapping, return 0.
- For `DefString`: record the string → offset mapping, return −1.

The `Applier` struct (lower.rs:810) and every `resolve_ref`/`eval_def_string` call site is
one of ~15 sites that resolve DefIndex/DefString. Each needs to go through the env.

---

## 4. AST hashing

### What to hash

The lowering input is: **(type name, parent name, flattened statement list)**. The output
(position-dependent) also depends on the symbol table (for `Expr::Symbol` resolution) and the
`LowerEnv` (for def references / strings). But the symbol table is a global input — any symbol
change potentially affects any def, which is too broad for fine-grained invalidation.

**Recommended strategy:** hash the AST of the def's own body (after flattening is excluded).
Include the type name and parent name in the key. The symbol table is treated as a global
input: a change to it invalidates everything. This is conservative (over-invalidates) but
correct, and since symbol-table changes are rare (header edits), the performance cost is
negligible.

### Span exclusion

`Spanned<T>: PartialEq` already ignores spans. We need a `Hash` impl that does the same.
The AST types are:

| Type | Fields for hashing |
|---|---|
| `Definition` | `is_template`, `def_type`, `name`, `specializes`, `body` (value only) |
| `Statement` / `Field` / `MethodCall` / `TaggedBlock` | All non-span fields |
| `Expr` | All variants, value-only (spans on sub-exprs in `BitOr`/`Add` ignored) |
| `Call` | `name`, `arguments` (value only) |
| `PathSegment` | `Field(String)` / `Index(Spanned<Expr>` — hash value of inner expr) |

### Approach

Add a `Hash` impl on each AST type (or use a custom `AstHasher` that skips span fields).
Span-exclusion can be tested by inserting a comment/whitespace and asserting the hash is
unchanged.

The simplest approach: derive `Hash` but exclude `Span` from the hash. Since `Spanned<T>: PartialEq` ignores span, we can add a custom `Hash` for `Spanned` that delegates to
`T::hash`:

```rust
impl<T: Hash> Hash for Spanned<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}
```

Then `#[derive(Hash)]` on `Definition`, `Statement`, `Field`, `MethodCall`, `TaggedBlock`,
`Call`, `PropertyPath`, `PathSegment`, `Expr`, `EnumDecl`, `EnumVariant`, `EnumExpr`,
`Define`, `Namespace`, `IfDef`, `Item` would work correctly.

---

## 5. Specialization graph

Currently, `flatten_specialization` walks the `specialises` chain on demand. This is called
**twice per def** (once for the named body, once for sub-defs during `build_subdefs`). The
immediate win (§9 AGENTS.md) is computing it once.

### Graph structure

Each def has at most one parent (`specialises`). This forms a forest (collection of trees).
The graph is not explicitly built; each call to `flatten_specialization` (lower.rs:158)
traverses the chain:

```rust
let mut chain: Vec<&Definition> = vec![def];
while let Some(parent_name) = &current.specializes {
    let parent = defs_by_name.get(parent_name.as_str())?;
    // cycle check
    chain.push(parent);
}
// Reverse so most-distant-ancestor-first
Ok(chain.iter().rev().flat_map(|d| d.body.iter().cloned()).collect())
```

### Building the graph explicitly

```rust
struct SpecGraph {
    /// def name → immediate children (reverse edges)
    children: HashMap<String, Vec<String>>,
    /// def name → chain from root (most-distant-ancestor-first), including self
    chains: HashMap<String, Vec<String>>,
}
```

Built once after `parse_corpus`, used by both flattening and cascade invalidation.

### Cascade invalidation

Editing a def's own body invalidates the def and all transitive descendants. The graph gives
us this for free: invalidate `def` plus `descendants(def)`.

---

## 6. redb database design

### Key space

```
"schema_version"        → u32              (bump on any defs-derive or lower.rs change)
"symbol_table_hash"     → u64              (hash of all symbol names + values)
"def:<name>"            → CachedDef        (per-def compiled body)
"file:<path>"           → FileEntry        (per-file parse result, optional layer)
```

### CachedDef value

```rust
struct CachedDef {
    /// Hash of the def's own AST (span-excluding), plus type name and parent name.
    ast_hash: u64,
    /// Type name (needed for `lower_def` dispatch and `ClassIndex`).
    def_type: String,
    /// The lowered body with placeholder indices/offsets.
    body_bytes: Vec<u8>,
    /// Relocations: (byte_offset, def_name) for each DefIndex field.
    def_refs: Vec<(u32, String)>,
    /// Relocations: (byte_offset, string) for each DefString field.
    def_strings: Vec<(u32, String)>,
}
```

### FileEntry value (optional second layer)

```rust
struct FileEntry {
    /// Hash of the file's text content.
    content_hash: u64,
    /// Serialized SourceAst (using a compact binary format, or just the text).
    /// The text alone is sufficient since parsing is only 10% of build time.
    text: String,
}
```

### Database location

`<output_dir>/.defcache/` or a configurable path. The cache is per-corpus (different input
directories have different caches).

---

## 7. The compile/link split in detail

### Compile phase (produces cacheable output)

Input: `Definition` (AST), `SymbolTable`, type's NULLDEF base.

1. `flatten_specialization` (use the pre-built graph) → `Vec<Spanned<Statement>>`.
2. `lower_def` with a **resolving LowerEnv** that records relocations instead of resolving:
   - `def_ref(name)` → record `(byte_offset_of_next_defindex, name)`, return 0
   - `def_string(s)` → record `(byte_offset_of_next_defstring, s)`, return −1
3. Serialize the lowered body (which produces the byte offsets for relocations).
4. Return `CachedDef { ast_hash, def_type, body_bytes, def_refs, def_strings }`.

**Problem:** the `LowerEnv` doesn't know the byte offset at the point of resolution. The
`DefIndex`/`DefString` value is set during `lower_def` and only serialized later. The byte
offset is known only after serialization.

**Solution A:** During lowering, use marker values for `DefIndex`/`DefString` in the
serialized output, then scan the serialized bytes afterward to find them and build the
relocation table. Fragile — depends on serialization being deterministic and marker values
not colliding with real data.

**Solution B (better):** During lowering, record the *logical* relocation: which field of
which struct was set to which name/string. Then at link time, walk the struct via reflection
and patch the resolved values in.

**Solution C (simplest):** Don't store `DefBody` in the cache at all. Store the **statement
list** (flattened) and re-lower at link time. Lowering is the expensive part (88% of build),
so this defeats the purpose... unless most defs are trivial (generic lowering) and re-lowering
a single def is cheap.

Wait — re-examining the numbers: the 3.09 s for game.bin lowering includes **all** 9,264
named defs + 33,525 sub-defs. Lowering ONE def is fast. The problem is the scale.

**Solution D (pragmatic):** Cache the `DefBody` with placeholder values, but instead of
tracking byte offsets, **re-resolve** the relocations by walking the struct with reflection at
link time. This requires a `patch_def_body` function that takes a `DefBody`, walks every
`DefIndex`/`DefString` field via `visit_fields`, and:
- For `DefIndex(0)`: look up the name in the relocation table, resolve to index, set.
- For `DefString(-1)`: look up the string in the relocation table, intern, set.

But we don't store WHICH name maps to WHICH field — we just have all-zero DefIndex slots. We
don't know which def name to resolve for each slot.

**Actually, we could store this:** use the reflection visitor to record the field name and
the symbolic value during lowering. The relocation table is **keyed by field path** (crc32 of
wire name), not by byte offset.

```rust
struct SymbolicBody {
    body: DefBody,
    /// Map from crc32(wire_field_name) to the def name it should resolve to.
    def_refs: HashMap<u32, String>,
    /// Map from crc32(wire_field_name) to the string it should intern.
    def_strings: HashMap<u32, String>,
}
```

At link time, walk the `DefBody` with a visitor, and for each field whose crc matches a key
in one of the maps, set the resolved value.

**This is the right approach.** It doesn't depend on serialization byte offsets and works with
the existing reflection infrastructure.

### Link phase (resolves symbolic → concrete)

1. Assign global indices to named defs (may have shifted).
2. For each cached body, walk fields via `visit_fields`, resolve `def_refs` and `def_strings`.
3. Intern strings into `NamesBuilder`.
4. Serialize.

---

## 8. flatten_specialization deduplication (immediate win)

Currently runs twice per def. The fix:

1. In `emit_nulldef_and_named`, save the flattened body to a side map.
2. In `build_subdefs`, use the saved body instead of re-computing.

```rust
// In build_one_bin:
let mut flattened_bodies: HashMap<String, Vec<Spanned<Statement>>> = HashMap::new();

for name in &named_order {
    let body = flatten_specialization(def, ctx.defs_by_name)?;
    flattened_bodies.insert(name.clone(), body.clone());
    // ... use body for named lowering ...
}

// In build_subdefs, use flattened_bodies instead of calling flatten_specialization again.
```

This is a no-brainer, safe (same input → same output), and the simplest thing to do first.

---

## 9. Recommended implementation sequence

### Step 0: flatten_specialization double-run fix
Small, safe, immediate win. Measure the time saved.

### Step 1: Build the specialization graph explicitly
- Add `SpecGraph` to `ParsedCorpus` or `build_one_bin`.
- `flatten_specialization` uses the precomputed chain.
- Cascade invalidation uses the reverse edges.

### Step 2: Add AST hashing
- Implement `Hash for Spanned<T>` (excludes span).
- Derive `Hash` on all AST types.
- Test: insert comment → hash unchanged.
- Hash the def's own body (not flattened), plus type name and parent name.

### Step 3: Restore the LowerEnv seam with relocation recording
- Define `LowerEnv` trait with `def_ref(name) -> Result<(), LowerError>` and
  `def_string(s) -> Result<(), LowerError>`.
- The recording implementation writes `(crc32(wire_name), name)` to a side `Vec` and
  returns 0/−1 for the DefIndex/DefString value.
- Thread it through `lower_def`, `lower_generic`, `Applier`, and all bespoke arms.
- Golden-gated: the linked output must be byte-identical to current. The recording env
  resolves the same values as today; it just defers the intern/resolve to link time.

### Step 4: Define CachedDef and link-time patching
- `SymbolicBody { body: DefBody, def_refs: HashMap<u32, String>, def_strings: HashMap<u32, String> }`.
- `patch_body(&mut DefBody, def_refs: &HashMap<u32, String>, def_indices: &HashMap<String, u32>, names: &mut NamesBuilder)`.
- Golden-gated: a "lower with recording, then patch" round-trip must produce the same bytes
  as direct lowering.

### Step 5: Add redb cache
- Database at `<output>/.defcache/`.
- Key: `def:<name>` → `CachedDef`.
- On build: for each def, compute AST hash, check cache. If hit and symbol table unchanged,
  skip lowering, use cached body. If miss, lower, store in cache.
- Cascade invalidation: if a def's AST hash changed, invalidate it + all descendants.
- Symbol table change → invalidate everything (for now; can be refined later).

### Step 6: File-hash layer (optional)
- Cache parsed `SourceAst` per file. Only 10% of build time, so this is low priority.
- Worth doing if the parse phase grows slower with larger modded corpora.

---

## 10. Risks and open questions

### Risk: relocation correctness
The `DefIndex(0)` sentinel is load-bearing — an unset reference is index 0, which resolves
to the NULLDEF of that class. If a relocation is missed during patching, the output has a
real 0 where it should have a resolved value. Golden tests catch this for the stock corpus,
but modded defs with new references could be affected.

**Mitigation:** After patching, verify that no `DefIndex` field still contains 0 when the
relocation table has an entry for that field's CRC. The `visit_fields` walk can check this.

### Risk: SymbolTable changes are coarse-grained
Any header edit invalidates every cached def. For a typical mod, this is fine (headers rarely
change). If it becomes a problem, track per-def symbol reads during lowering and only
invalidate defs that read changed symbols.

### Risk: Cache size
Serialized `DefBody` for all 9,264 named defs + 33,525 sub-defs could be large. But redb is
an on-disk database; the cache lives on disk, not in memory. Memory usage during build is
unchanged.

### Open question: sub-def caching
Sub-defs (tagged blocks) have their own lowering and dedup. Currently 33,525 sub-defs are
lowered per build, many duplicates. The `(tag, bytes)` dedup key means identical sub-defs
share one entry. For caching, sub-def lowering could be cached independently or as part of the
parent def's cache entry.

**Recommendation:** Cache sub-def bodies as part of the parent's `CachedDef`. When the
parent's AST hash is unchanged, all its sub-defs are unchanged too (since they come from
tagged blocks in the flattened body). When the parent changes, re-lower all its sub-defs.

### Open question: NamesBuilder interning order
The `names.bin` is shared across all three binaries. Cached defs from different binaries
intern strings in potentially different order. But since we intern at link time (after
index assignment), this is handled correctly: the `NamesBuilder` sees all strings from all
three binaries in the order they're emitted.

---

## 11. Key files to modify

| File | Changes |
|---|---|
| `packages/def-compiler/src/lower.rs` | `LowerEnv` trait, recording impl, threading through all arms |
| `packages/def-compiler/src/build.rs` | SpecGraph, cache lookup, link phase, `flatten_specialization` dedup |
| `packages/def-compiler/src/reader.rs` | Maybe nothing — `Evaluator` stays symbol-table-only |
| `packages/defs/src/text/mod.rs` | `Hash` derives on AST types |
| `packages/defs/src/text/base.rs` | `Hash for Spanned<T>` |
| `packages/def-compiler/Cargo.toml` | Add `redb` dependency |
| New: `packages/def-compiler/src/cache.rs` | `CachedDef`, `SymbolicBody`, `patch_body`, redb I/O |
| New: `packages/def-compiler/src/graph.rs` | `SpecGraph` — explicit specialization DAG |
