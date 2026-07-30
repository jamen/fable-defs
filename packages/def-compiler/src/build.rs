//! From-scratch def binary builder.
//!
//! Assembles `game.bin` / `frontend.bin` / `script.bin` (+ the shared
//! `names.bin`) with **our own** global-index allocation for the named and
//! sub-def regions. No retail binary is consulted.
//!
//! The pipeline has three phases:
//! 1. **[`parse_corpus`]** — parse every `.def`/`.tpl` file once, loading all
//!    header symbols, registering every source file, and producing the shared
//!    [`ParsedCorpus`].
//! 2. **[`build_one_bin`]** — for each binary, filter the corpus by manifest
//!    membership, allocate indices, lower, and serialize. Driven by a
//!    [`BinConfig`].
//! 3. **Finalize** — write `names.bin` from the shared [`NamesBuilder`].
//!
//! Diagnostics are **collected, not rendered**: everything the build has to say
//! about the corpus lands in [`BuildReport::diagnostics`] (or
//! [`BuildError::diagnostics`]) with the source spans intact, so the caller
//! owns presentation. [`BuildReport::sources`] carries the source text the
//! spans point into.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use defs::binary::{
    Chunk, ChunkIndex, ChunkIndexEntry, ChunkIndexHeader, DefBinary, DefBinaryHeader, DefBody,
    EntryPreamble, EntryRecord, NameRef, SubDefRecord, def_name_has_subdef_table,
};
use defs::crc32;
use defs::text::{
    DefFile, DefParseError, Definition, Expr, Span, Spanned, Statement, SymbolTable,
    TextParseErrorKind, header::parse_header_file, parse_def_file,
};
use defs::names::NamesBuilder;

use crate::lower::{LowerError, flatten_specialization, lower_def};
use crate::manifest;
use crate::walk_def_files;

// ═══════════════════════════════════════════════════════════════════════════════
//  Public API
// ═══════════════════════════════════════════════════════════════════════════════

/// One source file the build read, kept so diagnostic spans can be resolved
/// back to text by whoever renders them.
#[derive(Debug, Clone)]
pub struct SourceFile {
    /// Path as given on disk, with separators normalized to `/`.
    pub path: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Error,
}

/// A span within a diagnostic's source file, optionally annotated.
#[derive(Debug, Clone)]
pub struct DiagnosticLabel {
    /// `true` for the span the diagnostic is *about*; `false` for supporting
    /// context (e.g. "in this definition").
    pub primary: bool,
    pub span: Span,
    pub message: Option<String>,
}

/// One thing the build has to say about the corpus.
///
/// `source` indexes [`BuildReport::sources`] / [`BuildError::sources`]. It is
/// `None` for diagnostics with no registered source file (header files, which
/// are not retained), in which case `message` names the file itself.
#[derive(Debug, Clone)]
pub struct BuildDiagnostic {
    pub severity: Severity,
    pub message: String,
    pub source: Option<usize>,
    pub labels: Vec<DiagnosticLabel>,
}

impl BuildDiagnostic {
    fn bare(severity: Severity, message: String) -> Self {
        Self {
            severity,
            message,
            source: None,
            labels: Vec::new(),
        }
    }
}

/// Per-binary outcome.
#[derive(Debug, Clone)]
pub struct BinSummary {
    pub label: &'static str,
    pub file_name: &'static str,
    /// Named definitions successfully lowered.
    pub lowered: usize,
    /// Total entries written (NULLDEF + named + anonymous sub-defs).
    pub entries: u32,
    pub has_sub_defs: bool,
    pub sub_defs_lowered: usize,
    /// Distinct anonymous sub-def entries after `(tag, bytes)` dedup.
    pub sub_defs_unique: usize,
}

/// A successful build.
#[derive(Debug)]
pub struct BuildReport {
    pub sources: Vec<SourceFile>,
    /// Warnings only — any error fails the build (see [`BuildError`]).
    pub diagnostics: Vec<BuildDiagnostic>,
    pub bins: Vec<BinSummary>,
    pub elapsed: Duration,
}

impl BuildReport {
    pub fn warnings(&self) -> impl Iterator<Item = &BuildDiagnostic> {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Warning)
    }
}

/// A failed build. Carries the diagnostics collected before the failure so the
/// caller can report *why* it failed, not just that it did.
#[derive(Debug)]
pub struct BuildError {
    pub message: String,
    pub sources: Vec<SourceFile>,
    pub diagnostics: Vec<BuildDiagnostic>,
}

impl BuildError {
    fn bare(message: String) -> Self {
        Self {
            message,
            sources: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    pub fn errors(&self) -> impl Iterator<Item = &BuildDiagnostic> {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
    }
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BuildError {}

/// Progress events, emitted as the build proceeds so long builds can report
/// liveness. Purely informational — everything durable is in [`BuildReport`].
#[derive(Debug)]
pub enum Progress<'a> {
    FileParsed {
        path: &'a str,
        definitions: usize,
    },
    CompileStarted,
    Lowering {
        label: &'static str,
        named: usize,
    },
    BinFinished(&'a BinSummary),
}

/// Compile the text def corpus under `input` into the four binaries in
/// `output`, creating `output` if needed.
///
/// `input` is the `Defs/` directory: `.def`/`.tpl` sources plus the `.h` header
/// files they draw symbols from, scanned recursively.  `output` receives
/// `game.bin`, `frontend.bin`, `script.bin`, and `names.bin`.
pub fn build(input: &Path, output: &Path) -> Result<BuildReport, BuildError> {
    build_with_progress(input, output, &mut |_| {})
}

/// [`build`], plus a callback invoked as the build proceeds.
pub fn build_with_progress(
    input: &Path,
    output: &Path,
    on_progress: &mut dyn FnMut(Progress),
) -> Result<BuildReport, BuildError> {
    // Without this check a wrong input path is not an error: the directory walks
    // yield nothing, and the build cheerfully emits NULLDEF-only binaries that
    // look valid and load into a broken game.
    if !input.is_dir() {
        return Err(BuildError::bare(format!(
            "input is not a directory: {}",
            input.display()
        )));
    }
    std::fs::create_dir_all(output)
        .map_err(|e| BuildError::bare(format!("create out dir: {e}")))?;

    let started = Instant::now();
    let diagnostics = Diagnostics::default();

    let corpus = match parse_corpus(input, &diagnostics, on_progress) {
        Ok(c) => c,
        Err(message) => {
            return Err(BuildError {
                message,
                sources: Vec::new(),
                diagnostics: diagnostics.take(),
            });
        }
    };

    // From here on any failure can still point at source, so hand the caller
    // the sources and everything collected so far.
    let finish = |message: String, sources: Vec<SourceFile>, diagnostics: &Diagnostics| BuildError {
        message,
        sources,
        diagnostics: diagnostics.take(),
    };

    // A parse error is fatal for its file — every def in it is dropped. Letting
    // the build succeed anyway means silently emitting binaries that are missing
    // defs, which is exactly the failure the diagnostics exist to prevent.
    let parse_failures = diagnostics.count(Severity::Error);
    if parse_failures > 0 {
        return Err(finish(
            format!("{parse_failures} file(s) failed to parse"),
            corpus.sources,
            &diagnostics,
        ));
    }

    on_progress(Progress::CompileStarted);
    let names_cell = RefCell::new(NamesBuilder::new());
    let mut bins = Vec::with_capacity(3);
    for config in [&GAME_CONFIG, &FRONTEND_CONFIG, &SCRIPT_CONFIG] {
        match build_one_bin(&corpus, config, &diagnostics, &names_cell, output, on_progress) {
            Ok(summary) => {
                on_progress(Progress::BinFinished(&summary));
                bins.push(summary);
            }
            Err(message) => return Err(finish(message, corpus.sources, &diagnostics)),
        }
    }

    let names = names_cell.into_inner().finalize(manifest::NAMES_HEADER_BYTES);
    if let Err(e) = std::fs::write(output.join("names.bin"), names.to_bytes()) {
        return Err(finish(
            format!("write names.bin: {e}"),
            corpus.sources,
            &diagnostics,
        ));
    }

    Ok(BuildReport {
        sources: corpus.sources,
        diagnostics: diagnostics.take(),
        bins,
        elapsed: started.elapsed(),
    })
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Internal plumbing
// ═══════════════════════════════════════════════════════════════════════════════

/// Interior-mutable diagnostic sink, so the collection points can sit behind
/// the shared `&BuildCtx` borrows the pipeline is built on.
#[derive(Default)]
struct Diagnostics(RefCell<Vec<BuildDiagnostic>>);

impl Diagnostics {
    fn push(&self, diag: BuildDiagnostic) {
        self.0.borrow_mut().push(diag);
    }

    fn take(&self) -> Vec<BuildDiagnostic> {
        self.0.borrow_mut().split_off(0)
    }

    fn count(&self, severity: Severity) -> usize {
        self.0
            .borrow()
            .iter()
            .filter(|d| d.severity == severity)
            .count()
    }
}

/// Normalize `\` to `/` so path matching and reported paths are the same on
/// every platform. The scoping rules below match on `/`-separated fragments.
fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Recursively walk all statements/expressions in every definition's body
/// (including specialization-chain parents) and collect every bare symbol and
/// quoted string name. These names are candidate def references; template defs
/// whose name never appears in this set are unused and should not become binary
/// entries.
fn collect_body_references(
    files: &[&ParsedFile],
    defs_by_name: &HashMap<&str, &Definition>,
) -> HashSet<String> {
    fn walk_expr(expr: &Expr, out: &mut HashSet<String>) {
        match expr {
            Expr::String(s) | Expr::Symbol(s) => {
                out.insert(s.clone());
            }
            Expr::Constructor(call) => {
                for arg in &call.arguments {
                    walk_expr(&arg.value, out);
                }
            }
            Expr::BitOr(terms) | Expr::Add(terms) => {
                for t in terms {
                    walk_expr(&t.value, out);
                }
            }
            _ => {}
        }
    }
    fn walk_stmt(stmt: &Statement, out: &mut HashSet<String>) {
        match stmt {
            Statement::Field(f) => walk_expr(&f.expr.value, out),
            Statement::MethodCall(mc) => {
                for arg in &mc.call.arguments {
                    walk_expr(&arg.value, out);
                }
            }
            Statement::TaggedBlock(tb) => {
                for s in &tb.body {
                    walk_stmt(&s.value, out);
                }
            }
        }
    }
    fn walk_specialization_chain<'a>(
        def: &'a Definition,
        defs_by_name: &'a HashMap<&str, &Definition>,
        out: &mut HashSet<String>,
        visited: &mut HashSet<&'a str>,
    ) {
        if !visited.insert(def.name.as_str()) {
            return;
        }
        for stmt in &def.body {
            walk_stmt(&stmt.value, out);
        }
        if let Some(parent_name) = &def.specializes
            && let Some(parent) = defs_by_name.get(parent_name.as_str())
        {
            walk_specialization_chain(parent, defs_by_name, out, visited);
        }
    }
    let mut refs = HashSet::new();
    let mut visited = HashSet::new();
    for pf in files {
        for d in &pf.def_file.definitions {
            walk_specialization_chain(&d.value, defs_by_name, &mut refs, &mut visited);
        }
    }
    refs
}

/// Shared context built once per binary and threaded through the lowering
/// and emission pipeline.
struct BuildCtx<'a> {
    symbols: &'a SymbolTable,
    def_indices: &'a HashMap<String, u32>,
    names: &'a RefCell<NamesBuilder>,
    sources: &'a [SourceFile],
    def_to_source: &'a HashMap<String, usize>,
    def_spans: &'a HashMap<String, Span>,
    defs_by_name: &'a HashMap<&'a str, &'a Definition>,
    nulldefs: &'a HashMap<String, DefBody>,
    diagnostics: &'a Diagnostics,
}

struct ParsedCorpus {
    /// Parsed def files with their disk paths (for per-binary scoping).
    files: Vec<ParsedFile>,
    symbols: SymbolTable,
    sources: Vec<SourceFile>,
    def_to_source: HashMap<String, usize>,
    def_spans: HashMap<String, Span>,
}

struct ParsedFile {
    /// Normalized (`/`-separated) path; also the `sources` entry's path.
    path: String,
    def_file: DefFile,
}

/// Per-binary configuration: everything that distinguishes game.bin from
/// frontend.bin from script.bin. All membership data comes from the manifest.
struct BinConfig {
    label: &'static str,
    nulldef_entries: &'static [&'static str],
    binary_header: DefBinaryHeader,
    out_filename: &'static str,
    has_subdefs: bool,
    /// Exclude template defs that aren't referenced by any other def's body.
    /// Safe to enable for large binaries (game.bin); disable for small ones
    /// (frontend/script) where the engine may reference templates by name.
    filter_templates: bool,
    file_scope: fn(corpus: &[ParsedFile]) -> Vec<&ParsedFile>,
}

struct Built {
    def_name_off: u32,
    file_name_off: u32,
    counter: u32,
    preamble: EntryPreamble,
    sub_defs: Option<Vec<SubDefRecord>>,
    body: DefBody,
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Per-binary file scoping
// ═══════════════════════════════════════════════════════════════════════════════

fn game_file_scope(corpus: &[ParsedFile]) -> Vec<&ParsedFile> {
    corpus.iter().collect()
}

/// Retail's explicit frontend file list first, in its own order, then any other
/// file under `FrontEndDefs/` in walk order. The tail is what lets defs added
/// by an editor (which writes to its own file in that directory) reach
/// frontend.bin; the retail corpus has no such files, so ordering there is
/// unchanged.
fn frontend_file_scope(corpus: &[ParsedFile]) -> Vec<&ParsedFile> {
    let mut scoped: Vec<&ParsedFile> = manifest::FRONTEND_DEF_FILES
        .iter()
        .filter_map(|rel| corpus.iter().find(|pf| path_ends_with(&pf.path, rel)))
        .collect();
    for pf in corpus {
        if pf.path.contains("/FrontEndDefs/")
            && !scoped.iter().any(|s| std::ptr::eq(*s, pf))
        {
            scoped.push(pf);
        }
    }
    scoped
}

fn script_file_scope(corpus: &[ParsedFile]) -> Vec<&ParsedFile> {
    corpus
        .iter()
        .filter(|pf| pf.path.contains("ScriptDefs/"))
        .collect()
}

/// Whether a normalized path ends with a normalized relative path, **at a path
/// boundary**.
///
/// The boundary matters: the corpus has both `Defs/controls.def` and
/// `Defs/pc_controls.def`, and both `Defs/engine.def` and
/// `Defs/FrontEndDefs/engine.def`. A bare `str::ends_with` matches the wrong one
/// of each pair, and only picks the right file because the sorted walk order
/// happens to reach it first.
fn path_ends_with(path: &str, rel: &str) -> bool {
    match path.strip_suffix(rel) {
        Some("") => true,
        Some(prefix) => prefix.ends_with('/'),
        None => false,
    }
}

static GAME_CONFIG: BinConfig = BinConfig {
    label: "game",
    nulldef_entries: manifest::GAME_NULLDEF_ENTRIES,
    binary_header: manifest::GAME_HEADER,
    out_filename: "game.bin",
    has_subdefs: true,
    filter_templates: false,
    file_scope: game_file_scope,
};

static FRONTEND_CONFIG: BinConfig = BinConfig {
    label: "frontend",
    nulldef_entries: manifest::FRONTEND_NULLDEF_ENTRIES,
    binary_header: manifest::FRONTEND_HEADER,
    out_filename: "frontend.bin",
    has_subdefs: false,
    filter_templates: false,
    file_scope: frontend_file_scope,
};

static SCRIPT_CONFIG: BinConfig = BinConfig {
    label: "script",
    nulldef_entries: manifest::SCRIPT_NULLDEF_ENTRIES,
    binary_header: manifest::SCRIPT_HEADER,
    out_filename: "script.bin",
    has_subdefs: false,
    filter_templates: false,
    file_scope: script_file_scope,
};

// ═══════════════════════════════════════════════════════════════════════════════
//  Phase 1: Parse the entire corpus once
// ═══════════════════════════════════════════════════════════════════════════════

fn collect_h_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_h_files(&path, out);
        } else if matches!(path.extension().and_then(|e| e.to_str()), Some("h")) {
            let s = normalize_path(&path);
            if s.contains("/xbox/") || s.contains("scriptdialoguesnds2") {
                continue;
            }
            out.push(path);
        }
    }
}

fn load_symbols(source: &Path, diagnostics: &Diagnostics) -> SymbolTable {
    let mut symbols = SymbolTable::new();
    let mut h_files = Vec::new();
    collect_h_files(source, &mut h_files);
    h_files.sort();
    for path in &h_files {
        let display = normalize_path(path);
        match std::fs::read_to_string(path) {
            Ok(t) => match parse_header_file(&t) {
                Ok(hd) => {
                    if let Err(e) = symbols.evaluate(&hd) {
                        diagnostics.push(BuildDiagnostic::bare(
                            Severity::Warning,
                            format!("header {display}: evaluate error: {e:?}"),
                        ));
                    }
                }
                Err(e) => {
                    diagnostics.push(BuildDiagnostic::bare(
                        Severity::Warning,
                        format!("header {display}: parse error: {e}"),
                    ));
                }
            },
            Err(e) => {
                diagnostics.push(BuildDiagnostic::bare(
                    Severity::Warning,
                    format!("header {display}: read error: {e}"),
                ));
            }
        }
    }
    symbols
}

/// Engine enums the def-script parser registers in C++ (ECompositeBlendType,
/// _core/L4.hpp) that appear in no text header. The def-script uses the short
/// `BLEND_*` names (COMPOSITE_ prefix stripped). `BLEND_ALPHA = 2` is confirmed
/// against retail (every CHeroMorphDef TextureMorph blend); without it,
/// `TextureMorphs.Add(..., BLEND_ALPHA)` would silently lower the blend to 0.
fn inject_engine_enums(symbols: &mut SymbolTable) {
    let _ = symbols.insert("WATER_BUMP_PC", 0);
    for (name, value) in [
        ("BLEND_NULL", 0),
        ("BLEND_ADDITIVE", 1),
        ("BLEND_ALPHA", 2),
        ("BLEND_SOLID", 3),
        ("BLEND_MULTIPLY", 4),
    ] {
        let _ = symbols.insert(name, value);
    }
}

/// Parse every `.def` and `.tpl` file under `source`, loading all header
/// symbols.  Returns a [`ParsedCorpus`] that all three binary builders share.
fn parse_corpus(
    source: &Path,
    diagnostics: &Diagnostics,
    on_progress: &mut dyn FnMut(Progress),
) -> Result<ParsedCorpus, String> {
    let mut symbols = load_symbols(source, diagnostics);
    inject_engine_enums(&mut symbols);

    let mut sources: Vec<SourceFile> = Vec::new();
    let mut def_to_source: HashMap<String, usize> = HashMap::new();
    let mut def_spans: HashMap<String, Span> = HashMap::new();
    let mut parsed_files: Vec<ParsedFile> = Vec::new();
    let mut source_ids: Vec<usize> = Vec::new();

    for p in &walk_def_files(source) {
        if p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains("_deprecated."))
        {
            continue;
        }
        let raw = std::fs::read(p).map_err(|e| format!("read {p:?}: {e}"))?;
        let text = String::from_utf8_lossy(&raw).into_owned();
        let path = normalize_path(p);
        match parse_def_file(&text) {
            Ok(f) => {
                let def_count = f.definitions.len();
                let sid = sources.len();
                sources.push(SourceFile {
                    path: path.clone(),
                    text,
                });
                source_ids.push(sid);
                for d in &f.definitions {
                    def_to_source.insert(d.value.name.clone(), sid);
                    def_spans.insert(d.value.name.clone(), d.span);
                }
                on_progress(Progress::FileParsed {
                    path: &path,
                    definitions: def_count,
                });
                parsed_files.push(ParsedFile {
                    path,
                    def_file: f,
                });
            }
            Err(e) => {
                let sid = sources.len();
                sources.push(SourceFile { path, text });
                diagnostics.push(parse_error_diagnostic(&sources, sid, &e));
            }
        }
    }

    // An input directory that exists but holds no def sources would otherwise
    // produce NULLDEF-only binaries with no indication anything was wrong.
    //
    // Test `sources`, not `parsed_files`: a file that fails to parse is still
    // registered as a source, so this stays "found nothing to compile" and does
    // not swallow the "everything failed to parse" case, whose diagnostics are
    // far more useful to the caller.
    if sources.is_empty() {
        return Err(format!(
            "no .def or .tpl files found under {}",
            source.display()
        ));
    }

    for (&sid, pf) in source_ids.iter().zip(parsed_files.iter()) {
        if let Err(e) = symbols.evaluate_items(&pf.def_file.headers) {
            diagnostics.push(BuildDiagnostic {
                severity: Severity::Warning,
                message: format!("header evaluation failed: {e:?}"),
                source: Some(sid),
                labels: vec![DiagnosticLabel {
                    primary: false,
                    span: Span { start: 0, end: 0 },
                    message: Some("in this file".to_string()),
                }],
            });
        }
    }

    Ok(ParsedCorpus {
        files: parsed_files,
        symbols,
        sources,
        def_to_source,
        def_spans,
    })
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Diagnostic construction
// ═══════════════════════════════════════════════════════════════════════════════

fn def_header_end(text: &str, start: usize) -> usize {
    text[start..].find('\n').map_or(text.len(), |n| start + n)
}

/// A parse error is strict and fatal for its file, so the whole file's defs
/// vanish — it must always be surfaced. `MissingEndDefinition` points at the
/// unclosed definition; anything else gets a caret at the offending byte plus
/// an "in this definition" note.
fn parse_error_diagnostic(
    sources: &[SourceFile],
    source: usize,
    error: &DefParseError,
) -> BuildDiagnostic {
    let msg = format!("{error}");
    let text = &sources[source].text;
    let mut labels = Vec::new();
    match (&error.inner, error.def_header_pos) {
        (TextParseErrorKind::MissingEndDefinition, Some(def_pos)) => {
            labels.push(DiagnosticLabel {
                primary: true,
                span: Span {
                    start: def_pos,
                    end: def_header_end(text, def_pos),
                },
                message: Some("missing #end_definition for this definition".to_string()),
            });
        }
        (_, def_header) => {
            labels.push(DiagnosticLabel {
                primary: true,
                span: Span {
                    start: error.pos,
                    end: error.pos,
                },
                message: Some(msg.clone()),
            });
            if let Some(def_pos) = def_header {
                labels.push(DiagnosticLabel {
                    primary: false,
                    span: Span {
                        start: def_pos,
                        end: def_header_end(text, def_pos),
                    },
                    message: Some("in this definition".to_string()),
                });
            }
        }
    }
    BuildDiagnostic {
        severity: Severity::Error,
        message: msg,
        source: Some(source),
        labels,
    }
}

fn lowering_error_diagnostic(
    def_name: &str,
    def_type: &str,
    error: &LowerError,
    ctx: &BuildCtx,
) -> BuildDiagnostic {
    let source = ctx.def_to_source.get(def_name).copied();
    let Some(sid) = source else {
        return BuildDiagnostic::bare(
            Severity::Error,
            format!("{def_type} {def_name}: {error}"),
        );
    };
    let text = &ctx.sources[sid].text;
    let mut labels = Vec::new();
    if let Some(span) = error.primary_span() {
        labels.push(DiagnosticLabel {
            primary: true,
            span,
            message: Some(format!("{error}")),
        });
    }
    // Point the "in this definition" note at the name in the header line, not
    // the whole line, so the caret lands on what identifies the def.
    if let Some(dspan) = ctx.def_spans.get(def_name) {
        let header_line = &text[dspan.start..def_header_end(text, dspan.start)];
        if let Some(name_pos) = header_line.find(def_name) {
            let start = dspan.start + name_pos;
            labels.push(DiagnosticLabel {
                primary: false,
                span: Span {
                    start,
                    end: start + def_name.len(),
                },
                message: Some("in this definition".to_string()),
            });
        }
    }
    BuildDiagnostic {
        severity: Severity::Error,
        message: format!("{error}"),
        source,
        labels,
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Shared helper functions
// ═══════════════════════════════════════════════════════════════════════════════

/// Assign our own global indices to the named region: the first named entry
/// follows the `nulldef_count` NULLDEF entries, then one index per distinct
/// named def in first-seen corpus order (only defs the manifest lists as named
/// for this binary; the first occurrence of a duplicate name wins its slot).
fn collect_named(
    files: &[&ParsedFile],
    allowed_def_types: &HashSet<&str>,
    body_refs: Option<&HashSet<String>>,
    nulldef_count: u32,
) -> (Vec<String>, HashMap<String, u32>) {
    let mut named_order: Vec<String> = Vec::new();
    let mut named_indices: HashMap<String, u32> = HashMap::new();
    for pf in files {
        for d in &pf.def_file.definitions {
            if named_indices.contains_key(d.value.name.as_str()) {
                continue;
            }
            if !allowed_def_types.contains(d.value.def_type.as_str()) {
                continue;
            }
            if d.value.is_template
                && !body_refs
                    .as_ref()
                    .map(|r| r.contains(d.value.name.as_str()))
                    .unwrap_or(true)
            {
                continue;
            }
            let name = d.value.name.as_str();
            named_indices.insert(name.to_string(), nulldef_count + named_order.len() as u32);
            named_order.push(name.to_string());
        }
    }
    (named_order, named_indices)
}

fn build_nulldefs(
    classes: &[&str],
    symbols: &SymbolTable,
    def_indices: &HashMap<String, u32>,
    names: &RefCell<NamesBuilder>,
) -> Result<HashMap<String, DefBody>, String> {
    let mut map: HashMap<String, DefBody> = HashMap::new();
    for &dn in classes {
        if map.contains_key(dn) {
            continue;
        }
        // A NULLDEF body is `def_default()` with no statements applied, so this
        // can only fail if the type is missing from the schema entirely.
        let (body, _warnings) = lower_def(dn, None, &[], symbols, def_indices, names)
            .map_err(|e| format!("NULLDEF lowering failed for {dn}: {e}"))?;
        map.insert(dn.to_string(), body);
    }
    Ok(map)
}

fn emit_nulldef_and_named(
    entries: &mut Vec<Built>,
    nulldef_entries: &[&str],
    named_order: &[String],
    ctx: &BuildCtx,
) -> Result<usize, String> {
    let mut nulldef_counter: HashMap<String, u32> = HashMap::new();
    for &class_name in nulldef_entries {
        let fnm = format!("NULLDEF_{class_name}");
        let body = ctx
            .nulldefs
            .get(class_name)
            .ok_or_else(|| format!("NULLDEF body missing for {class_name}"))?
            .clone();
        let cc = nulldef_counter.entry(class_name.to_string()).or_insert(0);
        *cc += 1;
        let def_name_off = ctx.names.borrow_mut().intern(class_name);
        let file_name_off = ctx.names.borrow_mut().intern(&fnm);
        entries.push(Built {
            def_name_off,
            file_name_off,
            counter: *cc,
            preamble: EntryPreamble {
                is_real: false,
                is_template: false,
                unknown_0: 0,
            },
            sub_defs: if def_name_has_subdef_table(class_name) {
                Some(Vec::new())
            } else {
                None
            },
            body,
        });
    }

    let mut class_counter: HashMap<String, u32> = HashMap::new();
    let mut n_ok = 0;
    let mut error_count = 0;
    for name in named_order {
        let Some(def) = ctx.defs_by_name.get(name.as_str()) else {
            ctx.diagnostics.push(BuildDiagnostic::bare(
                Severity::Error,
                format!("definition {name} not found in parsed corpus"),
            ));
            error_count += 1;
            continue;
        };
        let def_type = def.def_type.clone();
        let def_name_off = ctx.names.borrow_mut().intern(&def_type);
        let file_name_off = ctx.names.borrow_mut().intern(name);

        let body = match flatten_specialization(def, ctx.defs_by_name) {
            Ok(b) => b,
            Err(e) => {
                ctx.diagnostics
                    .push(lowering_error_diagnostic(name, &def_type, &e, ctx));
                error_count += 1;
                continue;
            }
        };

        let (lowered, _warnings) = match lower_def(
            &def_type,
            ctx.nulldefs.get(def_type.as_str()).as_ref().copied(),
            &body,
            ctx.symbols,
            ctx.def_indices,
            ctx.names,
        ) {
            Ok(b) => {
                n_ok += 1;
                b
            }
            Err(e) => {
                ctx.diagnostics
                    .push(lowering_error_diagnostic(name, &def_type, &e, ctx));
                error_count += 1;
                continue;
            }
        };
        let cc = class_counter.entry(def_type.clone()).or_insert(0);
        *cc += 1;
        entries.push(Built {
            def_name_off,
            file_name_off,
            counter: *cc,
            preamble: EntryPreamble {
                is_real: true,
                is_template: false,
                unknown_0: 1,
            },
            sub_defs: if def_name_has_subdef_table(&def_type) {
                Some(Vec::new())
            } else {
                None
            },
            body: lowered,
        });
    }
    if error_count == 0 {
        Ok(n_ok)
    } else {
        Err(format!("{error_count} error(s)"))
    }
}

fn assemble_and_write(
    entries: Vec<Built>,
    header: &DefBinaryHeader,
    out_path: &Path,
    label: &str,
) -> Result<u32, String> {
    let entry_count = entries.len() as u32;
    let name_refs: Vec<NameRef> = entries
        .iter()
        .map(|e| NameRef {
            def_name_offset: e.def_name_off,
            file_name_offset: e.file_name_off,
            counter: e.counter,
        })
        .collect();
    let records: Vec<EntryRecord> = entries
        .into_iter()
        .map(|e| EntryRecord {
            preamble: e.preamble,
            sub_defs: e.sub_defs,
            chunk_start: 0,
            chunk_end: 0,
            body: e.body,
            raw_bytes: Vec::new(),
        })
        .collect();
    const TARGET: usize = 16384;
    let mut chunks = Vec::new();
    let mut entry_base = 0u32;
    let mut remaining = records;
    while !remaining.is_empty() {
        let mut sz = 0;
        let split = remaining
            .iter()
            .position(|e| {
                if sz > 0 && sz + e.byte_size() > TARGET {
                    true
                } else {
                    sz += e.byte_size();
                    false
                }
            })
            .unwrap_or(remaining.len());
        chunks.push(Chunk::from_entries(
            entry_base,
            remaining.drain(..split).collect(),
        ));
        entry_base += split as u32;
    }
    let hdr = DefBinaryHeader {
        use_names_bin: header.use_names_bin,
        file_indicator: header.file_indicator,
        platform_indicator: header.platform_indicator,
        entry_count,
    };
    let binary = DefBinary {
        header: hdr,
        name_refs,
        chunk_index: ChunkIndex {
            header: ChunkIndexHeader {
                chunk_count: chunks.len() as u32 + 1,
                reserved: 0,
            },
            entries: chunks
                .iter()
                .scan(0u32, |cum, c| {
                    *cum += c.entry_count;
                    Some(ChunkIndexEntry {
                        compressed_offset: 0,
                        cumulative_entry_count: *cum,
                    })
                })
                .collect(),
        },
        chunks,
    };
    std::fs::write(out_path, binary.to_bytes()).map_err(|e| format!("write {label}: {e}"))?;
    Ok(entry_count)
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Phase 2: Build one binary
// ═══════════════════════════════════════════════════════════════════════════════

/// Build the anonymous sub-def region (game.bin only).  Merges same-tag tagged
/// blocks across the specialization chain, lowers each sub-def, deduplicates
/// anonymous entries by (class-tag, bytes), and appends them to `entries`.
fn build_subdefs(
    named_order: &[String],
    named_base: usize,
    ctx: &BuildCtx,
    entries: &mut Vec<Built>,
) -> Result<(usize, usize), String> {
    let mut sub_dedup: HashMap<(String, Vec<u8>), u32> = HashMap::new();
    let mut sub_entries: Vec<Built> = Vec::new();
    let mut sub_counter: HashMap<String, u32> = HashMap::new();
    let (mut sub_ok, mut sub_fail) = (0, 0);
    for (oi, name) in named_order.iter().enumerate() {
        let owner_index = (named_base + oi) as u32;
        let Some(def) = ctx.defs_by_name.get(name.as_str()) else {
            continue;
        };
        if !def_name_has_subdef_table(&def.def_type) {
            continue;
        }
        let Ok(body) = flatten_specialization(def, ctx.defs_by_name) else {
            continue;
        };
        let mut blocks: HashMap<u32, (String, Vec<Spanned<Statement>>)> = HashMap::new();
        for st in &body {
            if let Statement::TaggedBlock(tb) = &st.value {
                blocks
                    .entry(crc32::crc(tb.tag.as_bytes()))
                    .and_modify(|(_, b)| b.extend(tb.body.iter().cloned()))
                    .or_insert_with(|| (tb.tag.clone(), tb.body.clone()));
            }
        }
        if blocks.is_empty() {
            continue;
        }
        let mut table: Vec<SubDefRecord> = Vec::new();
        let mut keys: Vec<u32> = blocks.keys().copied().collect();
        keys.sort();
        for k in keys {
            let (tag, blk) = &blocks[&k];
            let (lowered, _sub_warnings) = match lower_def(
                tag,
                ctx.nulldefs.get(tag.as_str()).as_ref().copied(),
                blk,
                ctx.symbols,
                ctx.def_indices,
                ctx.names,
            ) {
                Ok(b) => {
                    sub_ok += 1;
                    b
                }
                Err(e) => {
                    ctx.diagnostics.push(BuildDiagnostic {
                        severity: Severity::Error,
                        message: format!("sub-def lowering failed for <{tag}> in {name}: {e}"),
                        source: ctx.def_to_source.get(name.as_str()).copied(),
                        labels: Vec::new(),
                    });
                    sub_fail += 1;
                    continue;
                }
            };
            let mut bytes = vec![0u8; lowered.byte_size()];
            {
                let mut o = &mut bytes[..];
                if lowered.serialize(&mut o).is_err() {
                    continue;
                }
            }
            let sub_idx = *sub_dedup
                .entry((tag.clone(), bytes.clone()))
                .or_insert_with(|| {
                    let idx = sub_entries.len() as u32;
                    let cc = sub_counter.entry(tag.clone()).or_insert(0);
                    *cc += 1;
                    sub_entries.push(Built {
                        def_name_off: ctx.names.borrow_mut().intern(tag),
                        file_name_off: u32::MAX,
                        counter: *cc,
                        preamble: EntryPreamble {
                            is_real: true,
                            is_template: false,
                            unknown_0: 1,
                        },
                        sub_defs: if def_name_has_subdef_table(tag) {
                            Some(Vec::new())
                        } else {
                            None
                        },
                        body: lowered,
                    });
                    idx
                });
            table.push(SubDefRecord {
                name_crc: k,
                def_index: sub_idx,
                owner_index,
            });
        }
        entries[named_base + oi].sub_defs = Some(table);
    }
    if sub_fail > 0 {
        return Err(format!("{sub_fail} sub-def lowering error(s)"));
    }
    let sub_base = entries.len() as u32;
    for e in &mut entries[named_base..] {
        if let Some(table) = &mut e.sub_defs {
            for rec in table {
                rec.def_index += sub_base;
            }
        }
    }
    let unique = sub_entries.len();
    entries.extend(sub_entries);
    Ok((sub_ok, unique))
}

fn build_one_bin(
    corpus: &ParsedCorpus,
    config: &BinConfig,
    diagnostics: &Diagnostics,
    names: &RefCell<NamesBuilder>,
    out_dir: &Path,
    on_progress: &mut dyn FnMut(Progress),
) -> Result<BinSummary, String> {
    // Scope the corpus to this binary's file set, preserving the original
    // file-processing order (the game walk order for game.bin, the explicit
    // file-list order for frontend.bin, sorted-directory order for script.bin).
    let scoped = (config.file_scope)(&corpus.files);

    // Build the name→definition map from scoped files (last-wins by file order).
    let mut defs_by_name: HashMap<&str, &Definition> = HashMap::new();
    for pf in &scoped {
        for d in &pf.def_file.definitions {
            defs_by_name.insert(d.value.name.as_str(), &d.value);
        }
    }

    let allowed_def_types: HashSet<&str> = config.nulldef_entries.iter().copied().collect();
    let body_refs = if config.filter_templates {
        Some(collect_body_references(&scoped, &defs_by_name))
    } else {
        None
    };
    let nulldef_count = config.nulldef_entries.len() as u32;
    let (named_order, named_indices) = collect_named(
        &scoped,
        &allowed_def_types,
        body_refs.as_ref(),
        nulldef_count,
    );

    let nulldefs = build_nulldefs(
        config.nulldef_entries,
        &corpus.symbols,
        &named_indices,
        names,
    )?;
    let ctx = BuildCtx {
        symbols: &corpus.symbols,
        def_indices: &named_indices,
        names,
        sources: &corpus.sources,
        def_to_source: &corpus.def_to_source,
        def_spans: &corpus.def_spans,
        defs_by_name: &defs_by_name,
        nulldefs: &nulldefs,
        diagnostics,
    };

    on_progress(Progress::Lowering {
        label: config.label,
        named: named_order.len(),
    });
    let mut entries: Vec<Built> = Vec::new();
    let lowered = emit_nulldef_and_named(&mut entries, config.nulldef_entries, &named_order, &ctx)?;

    let named_base = nulldef_count as usize;
    let (sub_defs_lowered, sub_defs_unique) = if config.has_subdefs {
        build_subdefs(&named_order, named_base, &ctx, &mut entries)?
    } else {
        (0, 0)
    };

    let entries = assemble_and_write(
        entries,
        &config.binary_header,
        &out_dir.join(config.out_filename),
        config.label,
    )?;
    Ok(BinSummary {
        label: config.label,
        file_name: config.out_filename,
        lowered,
        entries,
        has_sub_defs: config.has_subdefs,
        sub_defs_lowered,
        sub_defs_unique,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_matching_respects_path_boundaries() {
        // The two real ambiguities in the corpus.
        assert!(path_ends_with("x/Defs/controls.def", "controls.def"));
        assert!(!path_ends_with("x/Defs/pc_controls.def", "controls.def"));
        assert!(path_ends_with(
            "x/Defs/FrontEndDefs/engine.def",
            "FrontEndDefs/engine.def"
        ));
        assert!(!path_ends_with("x/Defs/engine.def", "FrontEndDefs/engine.def"));
        // A bare relative path with no leading directory still matches at root.
        assert!(path_ends_with("ui_dialogs.def", "ui_dialogs.def"));
    }

    #[test]
    fn windows_separators_normalize_to_forward_slashes() {
        let path = normalize_path(Path::new(r"C:\Fable\Data\Defs\FrontEndDefs\engine.def"));
        assert_eq!(path, "C:/Fable/Data/Defs/FrontEndDefs/engine.def");
        // The scoping rules are written against `/`, so they now work on paths
        // that came from a Windows filesystem.
        assert!(path_ends_with(&path, "FrontEndDefs/engine.def"));
        assert!(normalize_path(Path::new(r"a\Defs\ScriptDefs\quest.def")).contains("ScriptDefs/"));
        assert!(normalize_path(Path::new(r"a\DevHeaders\xbox\gfx.h")).contains("/xbox/"));
    }
}
