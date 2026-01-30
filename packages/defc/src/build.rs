//! From-scratch def binary builder.
//!
//! Assembles `game.bin` / `frontend.bin` / `script.bin` (+ the shared
//! `names.bin`) with **our own** global-index allocation for the named and
//! sub-def regions, resolving every reference through a [`ScratchEnv`] backed by
//! that allocation. No retail binary is consulted.
//!
//! The pipeline has three phases:
//! 1. **[`parse_corpus`]** — parse every `.def`/`.tpl` file once, loading all
//!    header symbols, building the unified diagnostics store, and producing the
//!    shared [`ParsedCorpus`].
//! 2. **[`build_one_bin`]** — for each binary, filter the corpus by manifest
//!    membership, allocate indices, lower, and serialize. Driven by a
//!    [`BinaryConfig`].
//! 3. **Finalize** — write `names.bin` from the shared [`NamesBuilder`].

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use codespan_reporting::diagnostic::{Diagnostic, Label};
use codespan_reporting::files::{Files, SimpleFiles};
use codespan_reporting::term::{self, termcolor::StandardStream, Styles, StylesWriter};
use defs::crc32;
use defs::def::binary::{
    Chunk, ChunkIndex, ChunkIndexEntry, ChunkIndexHeader, DefBinary, DefBinaryHeader, DefBody,
    EntryPreamble, EntryRecord, NameRef, SubDefRecord, def_name_has_subdef_table,
};
use defs::names::{Names, NamesEntry};
use defs::def::text::{
    DefFile, DefParseError, Definition, Expr, Span, Spanned, Statement, SymbolTable, TextParseErrorKind,
    header::parse_header_file, parse_def_file,
};
use def_compiler::{
    LowerEnv, LowerError, flatten_specialization, lower_def, walk_def_files,
};


fn emit_diagnostic(files: &SimpleFiles<String, String>, diag: &Diagnostic<usize>) {
    let writer = StandardStream::stderr(termcolor::ColorChoice::Auto);
    let config = term::Config::default();
    let styles = Styles::default();
    let _ = term::emit_to_write_style(
        &mut StylesWriter::new(writer, &styles),
        &config,
        files,
        diag,
    );
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
        if let Some(parent_name) = &def.specializes {
            if let Some(parent) = defs_by_name.get(parent_name.as_str()) {
                walk_specialization_chain(parent, defs_by_name, out, visited);
            }
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

// ═══════════════════════════════════════════════════════════════════════════════
//  Pipeline types
// ═══════════════════════════════════════════════════════════════════════════════

/// Interns strings into the shared `names.bin` table, assigning each a stable
/// offset. All three binaries share one instance (names.bin is a single table).
struct NamesBuilder {
    map: BTreeMap<u32, NamesEntry>,
    off_of: HashMap<String, u32>,
    pos: usize,
}

impl NamesBuilder {
    fn new() -> Self {
        Self {
            map: BTreeMap::new(),
            off_of: HashMap::new(),
            pos: 20,
        }
    }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&o) = self.off_of.get(s) {
            return o;
        }
        let off = (self.pos + 4 - 20) as u32;
        self.map.insert(
            off,
            NamesEntry {
                crc: crc32::crc(s.as_bytes()),
                string: s.to_string(),
            },
        );
        self.off_of.insert(s.to_string(), off);
        self.pos += 4 + s.len() + 1;
        off
    }

    /// Consume the builder and produce a [`Names`] with pre-computed header
    /// metadata (StringCount + StreamLength at offsets 8/12). The pre-existing
    /// `NAMES_HEADER_BYTES` constant supplies the fixed magic + platform bytes.
    fn finalize(self, header_bytes: [u8; 20]) -> Names {
        let mut names = Names {
            header_bytes,
            map: self.map,
        };
        let bytes = names.to_bytes();
        let string_count = names.map.len() as u32;
        let stream_len = (bytes.len() - 16) as u32;
        names.header_bytes[8..12].copy_from_slice(&string_count.to_le_bytes());
        names.header_bytes[12..16].copy_from_slice(&stream_len.to_le_bytes());
        names
    }
}

/// Reference-resolution environment for lowering: named defs resolve to their
/// (our-own) global index; def-strings intern into the shared name table.
struct ScratchEnv<'n> {
    def_indices: HashMap<String, u32>,
    names: &'n RefCell<NamesBuilder>,
}

impl<'n> ScratchEnv<'n> {
    fn new(def_indices: HashMap<String, u32>, names: &'n RefCell<NamesBuilder>) -> Self {
        Self { def_indices, names }
    }

    fn intern(&self, s: &str) -> u32 {
        self.names.borrow_mut().intern(s)
    }
}

impl<'n> LowerEnv for ScratchEnv<'n> {
    fn def_index(&self, name: &str) -> Option<u32> {
        self.def_indices.get(name).copied()
    }
    fn def_string_offset(&self, string: &str) -> Option<u32> {
        Some(self.intern(string))
    }
}

/// Everything parsed from the text corpus — shared across all three binaries.
struct ParsedCorpus {
    /// Parsed def files with their disk paths (for per-binary scoping).
    files: Vec<ParsedFile>,
    symbols: SymbolTable,
    code_files: SimpleFiles<String, String>,
    def_to_file_id: HashMap<String, usize>,
    def_spans: HashMap<String, Span>,
}

struct ParsedFile {
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

/// Shared context built once per binary and threaded through the lowering
/// and emission pipeline. Bundles the pass-through references so function
/// signatures stay compact.
struct BuildCtx<'a> {
    symbols: &'a SymbolTable,
    env: &'a ScratchEnv<'a>,
    code_files: &'a SimpleFiles<String, String>,
    def_to_file_id: &'a HashMap<String, usize>,
    def_spans: &'a HashMap<String, Span>,
    defs_by_name: &'a HashMap<&'a str, &'a Definition>,
    nulldefs: &'a HashMap<String, DefBody>,
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
//  Config constants
// ═══════════════════════════════════════════════════════════════════════════════

/// Files that constitute the frontend corpus (relative to Defs/, forward slashes).
const FRONTEND_DEF_FILES: &[&str] = &[
    "ui_dialogs.def",
    "FrontEndDefs/engine.def",
    "FrontEndDefs/engine_video_options.def",
    "FrontEndDefs/front_end.def",
    "FrontEndDefs/frontend_test.def",
    "FrontEndDefs/pc_frontend.def",
    "config_options_defaults.def",
    "controls.def",
    "pc_controls.def",
];

fn game_file_scope(corpus: &[ParsedFile]) -> Vec<&ParsedFile> {
    corpus.iter().collect()
}

fn frontend_file_scope(corpus: &[ParsedFile]) -> Vec<&ParsedFile> {
    FRONTEND_DEF_FILES
        .iter()
        .filter_map(|rel| corpus.iter().find(|pf| pf.path.ends_with(rel)))
        .collect()
}

fn script_file_scope(corpus: &[ParsedFile]) -> Vec<&ParsedFile> {
    corpus
        .iter()
        .filter(|pf| pf.path.contains("ScriptDefs/") || pf.path.contains("ScriptDefs\\"))
        .collect()
}

const GAME_HEADER: DefBinaryHeader = DefBinaryHeader {
    use_names_bin: false,
    file_indicator: 0xC69C21A6,
    platform_indicator: 0xA8E36C34,
    entry_count: 0,
};
const FRONTEND_HEADER: DefBinaryHeader = DefBinaryHeader {
    use_names_bin: false,
    file_indicator: 0xE86E4CDE,
    platform_indicator: 0xA8E36C34,
    entry_count: 0,
};
const SCRIPT_HEADER: DefBinaryHeader = DefBinaryHeader {
    use_names_bin: false,
    file_indicator: 0x35A0BC99,
    platform_indicator: 0xA8E36C34,
    entry_count: 0,
};
const NAMES_HEADER_BYTES: [u8; 20] = [
    0x1E, 0xAB, 0x07, 0x00, 0x34, 0x6C, 0xE3, 0xA8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,
];

const GAME_NULLDEF_ENTRIES: &[&str] = &["ARMOUR","ATTACK_PATTERN","BRAIN","BUILDING","CAICreatureWillPowerIndicatorDef","CAIScratchpadDef","CAMERA_MANAGER","CAMERA_MANAGER_SET","CAMERA_MODE","CARRY_SLOT","CAbilityDef","CActionUseDef","CActivateQuestDef","CAnimatingObjectDef","CAppearanceDef","CAppearanceModifierDef","CAreaOfEffectAttackDef","CAugmentationDef","CBalverineBattleDef","CBedDef","CBettingDef","CBoastingPodiumDef","CBonusItemDef","CBossDef","CBriarRoseDef","CBuyHouseDef","CBuyableHouseDef","CCameraCollisionDef","CCarriedReadableDef","CCarryableDef","CCarryingDef","CChestDef","CClockDef","CCoinGameObstacleDef","CCombatAbilityBlockCounterAttackDef","CCombatAbilityBlockHeavyWeaponAttackDef","CCombatAbilityBlockLightWeaponAttackDef","CCombatAbilityBlockProjectileWeaponAttackDef","CCombatAbilityBlockUnarmedAttackDef","CCombatAbilityFlourishCounterAttackDef","CCombatAbilityGetHitCounterAttackDef","CCombatAbilityStrafeDef","CCombatAbilityUseProjectileWeaponDef","CContainerRewardHeroDef","CContextSensitiveItemDef","CCoopSpiritDef","CCrateStackDef","CCreatureDef","CCreatureGeneratorDef","CCreatureModeDef","CCreatureNavigationDef","CCreatureStatsDef","CDecapitationDef","CDegradableDef","CDoorDef","CDragonActionHoverDef","CDragonActionNapalmDef","CDragonActionSwoopDef","CDrunkennessDef","CEnemyDef","CEntitySoundDef","CExperienceDef","CExplodingObjectDef","CExplosionDef","CExplosiveTrailDef","CExpressionSubDef","CFireballSpellLevelDef","CFireheartMinigameDef","CFishDef","CFishingDef","CFishingRodDef","CFlammableDef","CGiftDef","CGoldDef","CGuardDef","CGuildMasterDef","CHairCardDef","CHasNameDef","CHeroCentreDef","CHeroDef","CHeroExperienceDef","CHeroMarriageDef","CHeroMorphDef","CHeroPostcardGeneratorDef","CHeroSpecialMovementDef","CHeroSuitDef","CHeroTitleDef","CHighlightItemDef","CHitLocationsDef","CIdleSchedulerDef","CInterestingToVillagersDef","CInventoryItemDef","CJackDragonDef","CJackOfBladesBattleDef","CKickableDef","CKrakenDef","CKrakenTentacleDef","CLightDef","CLightningOrbDef","CLookDef","CMazeBattleDef","CMultiStaticMeshDef","CNymphDef","COMBAT_DIALOGUE_DEF","COMBAT_SEQUENCE","COMBAT_TYPE","CONFIG_OPTIONS_DEFAULTS_DEF","CONTROL_SCHEME","CObjectAugmentationsDef","COccupiableDef","COpinionOfHeroDef","COracleMinigameDef","COverheadDisplayDef","CParticleAttacherDef","CPerceivedThingDef","CPhysicsDef","CQuestCardDef","CREATURE","CREATURE_ABILITY","CREATURE_GENERATION_FAMILY","CReadableDef","CReplaceableMeshDef","CResurrectionItemDef","CRumbleDef","CScorpionKingBattleDef","CShipDef","CShopDef","CShopItemDef","CSkeletalMorphDef","CSmashableDef","CSmokeGeneratorDef","CSnowTrollDef","CSoundAtmospheresDef","CSpecialAbilitiesDrainLifeDataDef","CSpecialAbilitiesForcePushDataDef","CSpecialEffectsDef","CSpotLightDef","CStealthDef","CStockItemDef","CSummonDef","CSummonableCreatureDef","CSummonerDef","CTCVolumeContainmentTrackerDef","CTargetingDef","CTattooDef","CTavernDef","CTavernGameCardBaseDef","CTavernGameCoinBaseDef","CTavernGameCoinGolfDef","CTavernGameDef","CTavernGameShoveHaPennyDef","CTavernGameSpotTheAdditionDef","CTavernTableDef","CTeleporterDef","CTextureReplacementDef","CThingDrainLifeShotDef","CThingMultiArrowShotDef","CThunderBattleDef","CTimeAppearanceFadeDef","CTrapDef","CTrollBattleDef","CTrophyDef","CTurncoatDef","CVillageDef","CVillageMemberDef","CVillagePeopleDef","CWallMountEffectsDef","CWaspQueenBattleDef","CWeaponDef","CWhisperBattleDef","CWifeDef","CWillResponseDef","ENGINE","ENGINE_THEME","ENGINE_THEME_GROUP","ENGINE_VIDEO_OPTIONS","ENVIRONMENT","ENVIRONMENT_THEME_DAY","EXPRESSION","FACTION","GLOBAL","HERO_ABILITY","HERO_COMBAT","HERO_MELEE_COMBAT_ABILITY","HERO_STATS","HIT_LOCATION","HOLY_SITE","INVENTORY_CATEGORY","INVENTORY_ITEM","INVENTORY_TYPE","LIGHTNING","LOCAL_DETAIL_GENERATOR","MARKER","MATERIAL","MELEE_COMBAT_KNOCKDOWN_EFFECTS","MESSAGE_EVENT","NOISE","OBJECT","OBJECT_FAMILY","OPINION_DEED_EFFECTS","OPINION_DEED_MASK","OPINION_PERSONALITY","OPINION_REACTION_MANAGER","OPINION_REACTION_MASK","OPINION_SOURCE","PHYSICAL_SWITCH","PLAYER","PLAYER_GUI","PLAYER_INVENTORY","PLAYER_MOVEMENT","REGION","SHOT","SIM_BUILDING","SIM_VOICES","SKY","SOUND_SETUP","SOUND_THEME","SPECIAL_ABILITIES_ASSASSIN_RUSH_DEF","SPECIAL_ABILITIES_BATTLE_CHARGE_DEF","SPECIAL_ABILITIES_BERSERK_DEF","SPECIAL_ABILITIES_BULLET_TIME_DEF","SPECIAL_ABILITIES_BURNT_EFFECT_DEF","SPECIAL_ABILITIES_CREATURE_TINT_DEF","SPECIAL_ABILITIES_DIVINE_WRATH_DEF","SPECIAL_ABILITIES_DRAIN_LIFE_DEF","SPECIAL_ABILITIES_DRUNKENNESS_DEF","SPECIAL_ABILITIES_ELECTROCUTED_EFFECT_DEF","SPECIAL_ABILITIES_ENFLAME_DEF","SPECIAL_ABILITIES_FIREBALL_SPELL_DEF","SPECIAL_ABILITIES_FORCE_PUSH_DEF","SPECIAL_ABILITIES_GHOST_SWORD_DEF","SPECIAL_ABILITIES_HEAL_LIFE_DEF","SPECIAL_ABILITIES_LIGHTNING_SPELL_DEF","SPECIAL_ABILITIES_MULTI_ARROW_DEF","SPECIAL_ABILITIES_MULTI_STRIKE_DEF","SPECIAL_ABILITIES_PHYSICAL_SHIELD_DEF","SPECIAL_ABILITIES_SUMMON_SPELL_DEF","SPECIAL_ABILITIES_THUNDER_LIGHTNING_STORM_DEF","SPECIAL_ABILITIES_TURNCOAT_SPELL_DEF","SPECIAL_ABILITIES_UNHOLY_POWER_DEF","SWITCH","THING","THING_GROUP","UI","UI_ICONS_DEF","UI_LOCALE_GRAPHICS_DEF","UI_MISC_THINGS_DEF","VILLAGE","VILLAGER_INTERACTION"];

const FRONTEND_NULLDEF_ENTRIES: &[&str] = &["CONFIG_OPTIONS_DEFAULTS_DEF","CONTROL_SCHEME","CONTROL_SCHEME","ENGINE","ENGINE_VIDEO_OPTIONS","FRONT_END","UI","UI_ICONS_DEF","UI_MISC_THINGS_DEF"];

const SCRIPT_NULLDEF_ENTRIES: &[&str] = &["CCutsceneDef","CRegionScriptDef","CScriptDef"];

static GAME_CONFIG: BinConfig = BinConfig {
    label:            "game",
    nulldef_entries:  GAME_NULLDEF_ENTRIES,
    binary_header:    GAME_HEADER,
    out_filename:     "game.bin",
    has_subdefs:      true,
    filter_templates: false,
    file_scope:       game_file_scope,
};

static FRONTEND_CONFIG: BinConfig = BinConfig {
    label:            "frontend",
    nulldef_entries:  FRONTEND_NULLDEF_ENTRIES,
    binary_header:    FRONTEND_HEADER,
    out_filename:     "frontend.bin",
    has_subdefs:      false,
    filter_templates: false,
    file_scope:       frontend_file_scope,
};

static SCRIPT_CONFIG: BinConfig = BinConfig {
    label:            "script",
    nulldef_entries:  SCRIPT_NULLDEF_ENTRIES,
    binary_header:    SCRIPT_HEADER,
    out_filename:     "script.bin",
    has_subdefs:      false,
    filter_templates: false,
    file_scope:       script_file_scope,
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
            let s = path.to_string_lossy();
            if s.contains("xbox/") || s.contains("scriptdialoguesnds2") {
                continue;
            }
            out.push(path);
        }
    }
}

fn load_symbols(source: &Path) -> SymbolTable {
    let mut symbols = SymbolTable::new();
    let mut h_files = Vec::new();
    collect_h_files(source, &mut h_files);
    h_files.sort();
    for path in &h_files {
        match std::fs::read_to_string(path) {
            Ok(t) => match parse_header_file(&t) {
                Ok(hd) => {
                    if let Err(e) = symbols.evaluate(&hd) {
                        eprintln!("warning: header {}: evaluate error: {e:?}", path.display());
                    }
                }
                Err(e) => {
                    eprintln!("warning: header {}: parse error: {e}", path.display());
                }
            },
            Err(e) => {
                eprintln!("warning: header {}: read error: {e}", path.display());
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
fn parse_corpus(source: &Path) -> Result<ParsedCorpus, String> {
    let mut symbols = load_symbols(source);
    inject_engine_enums(&mut symbols);

    let mut code_files: SimpleFiles<String, String> = SimpleFiles::new();
    let mut def_to_file_id: HashMap<String, usize> = HashMap::new();
    let mut def_spans: HashMap<String, Span> = HashMap::new();
    let mut parsed_files: Vec<ParsedFile> = Vec::new();
    let mut file_ids: Vec<usize> = Vec::new();

    for p in &walk_def_files(source) {
        if p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains("_deprecated."))
        {
            continue;
        }
        let raw = std::fs::read(p).map_err(|e| format!("read {p:?}: {e}"))?;
        let text = String::from_utf8_lossy(&raw).into_owned();
        let path_str = p.to_string_lossy().to_string();
        match parse_def_file(&text) {
            Ok(f) => {
                let def_count = f.definitions.len();
                let fid = code_files.add(path_str.clone(), text);
                file_ids.push(fid);
                for d in &f.definitions {
                    def_to_file_id.insert(d.value.name.clone(), fid);
                    def_spans.insert(d.value.name.clone(), d.span);
                }
                parsed_files.push(ParsedFile { path: path_str, def_file: f });
                eprintln!("    {} ({} definitions)", p.display(), def_count);
            }
            Err(e) => {
                let fid = code_files.add(path_str, text);
                render_parse_error(&code_files, fid, &e);
            }
        }
    }

    for (&file_id, pf) in file_ids.iter().zip(parsed_files.iter()) {
        if let Err(e) = symbols.evaluate_items(&pf.def_file.headers) {
            let diag = Diagnostic::warning()
                .with_message(format!("header evaluation failed: {e:?}"))
                .with_labels(vec![
                    Label::secondary(file_id, 0..0).with_message("in this file"),
                ]);
            emit_diagnostic(&code_files, &diag);
        }
    }

    Ok(ParsedCorpus {
        files: parsed_files,
        symbols,
        code_files,
        def_to_file_id,
        def_spans,
    })
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Shared helper functions
// ═══════════════════════════════════════════════════════════════════════════════



fn def_header_end(text: &str, start: usize) -> usize {
    text[start..]
        .find('\n')
        .map_or(text.len(), |n| start + n)
}

fn render_parse_error(
    files: &SimpleFiles<String, String>,
    file_id: usize,
    error: &DefParseError,
) {
    let Ok(source) = files.source(file_id) else { return };
    let msg = format!("{error}");
    let mut labels = Vec::new();
    match (&error.inner, error.def_header_pos) {
        (TextParseErrorKind::MissingEndDefinition, Some(def_pos)) => {
            let def_end = def_header_end(source, def_pos);
            labels.push(
                Label::primary(file_id, def_pos..def_end)
                    .with_message("missing #end_definition for this definition"),
            );
        }
        (_, def_header) => {
            labels.push(
                Label::primary(file_id, error.pos..error.pos).with_message(msg.clone()),
            );
            if let Some(def_pos) = def_header {
                let def_end = def_header_end(source, def_pos);
                labels.push(
                    Label::secondary(file_id, def_pos..def_end)
                        .with_message("in this definition"),
                );
            }
        }
    }
    let diag = Diagnostic::error().with_message(msg).with_labels(labels);
    emit_diagnostic(files, &diag);
}

fn render_lowering_error(
    def_name: &str,
    def_type: &str,
    error: &LowerError,
    file_id: Option<usize>,
    def_span: Option<&Span>,
    files: &SimpleFiles<String, String>,
) {
    let Some(fid) = file_id else {
        eprintln!("error: {def_type} {def_name}: {error}");
        return;
    };
    let Ok(text) = files.source(fid) else {
        eprintln!("error: {def_type} {def_name}: {error}");
        return;
    };
    let expr_span = error.primary_span();
    let mut labels = Vec::new();
    if let Some(span) = expr_span {
        labels.push(
            Label::primary(fid, span.start..span.end)
                .with_message(format!("{error}")),
        );
    }
    if let Some(dspan) = def_span {
        let header_end = def_header_end(text, dspan.start);
        let header_line = &text[dspan.start..header_end];
        if let Some(name_pos) = header_line.find(def_name) {
            let name_start = dspan.start + name_pos;
            let name_end = name_start + def_name.len();
            labels.push(
                Label::secondary(fid, name_start..name_end)
                    .with_message("in this definition"),
            );
        }
    }
    let diag = Diagnostic::error()
        .with_message(format!("{error}"))
        .with_labels(labels);
    emit_diagnostic(files, &diag);
}

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
    env: &ScratchEnv,
) -> HashMap<String, DefBody> {
    let mut map: HashMap<String, DefBody> = HashMap::new();
    for &dn in classes {
        if map.contains_key(dn) {
            continue;
        }
        let (body, _warnings) = lower_def(dn, None, &[], symbols, env).expect("NULLDEF lowering should never fail");
        map.insert(dn.to_string(), body);
    }
    map
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
        let body = ctx.nulldefs
            .get(class_name)
            .expect("NULLDEF body should be pre-built")
            .clone();
        let cc = nulldef_counter.entry(class_name.to_string()).or_insert(0);
        *cc += 1;
        entries.push(Built {
            def_name_off: ctx.env.intern(class_name),
            file_name_off: ctx.env.intern(&fnm),
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
            eprintln!("error: definition {name} not found in parsed corpus");
            error_count += 1;
            continue;
        };
        let def_type = def.def_type.clone();
        let def_name_off = ctx.env.intern(&def_type);
        let file_name_off = ctx.env.intern(name);

        let body = match flatten_specialization(def, ctx.defs_by_name) {
            Ok(b) => b,
            Err(e) => {
                render_lowering_error(
                    name,
                    &def_type,
                    &e,
                    ctx.def_to_file_id.get(name.as_str()).copied(),
                    ctx.def_spans.get(name.as_str()),
                    ctx.code_files,
                );
                error_count += 1;
                continue;
            }
        };

        let fid = ctx.def_to_file_id.get(name.as_str()).copied();
        let dspan = ctx.def_spans.get(name.as_str());

        let (lowered, _warnings) = match lower_def(
            &def_type,
            ctx.nulldefs.get(def_type.as_str()).as_ref().copied(),
            &body,
            ctx.symbols,
            ctx.env,
        ) {
            Ok(b) => {
                n_ok += 1;
                b
            }
            Err(e) => {
                render_lowering_error(name, &def_type, &e, fid, dspan, ctx.code_files);
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
fn build_subdefs<'a>(
    named_order: &[String],
    named_base: usize,
    ctx: &BuildCtx<'a>,
    entries: &mut Vec<Built>,
) -> Result<(usize, usize, usize), String> {
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
                ctx.env,
            ) {
                Ok(b) => {
                    sub_ok += 1;
                    b
                }
                Err(e) => {
                    if let Some(&fid) = ctx.def_to_file_id.get(name.as_str()) {
                        let diag = Diagnostic::error()
                            .with_message(format!(
                                "sub-def lowering failed for <{tag}> in {name}"
                            ))
                            .with_labels(vec![Label::secondary(fid, 0..0)
                                .with_message(format!("{e}"))]);
                        emit_diagnostic(ctx.code_files, &diag);
                    }
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
                        def_name_off: ctx.env.intern(tag),
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
    Ok((sub_ok, sub_fail, unique))
}

fn build_one_bin(
    corpus: &ParsedCorpus,
    config: &BinConfig,
    names: &RefCell<NamesBuilder>,
    out_dir: &Path,
) -> Result<u32, String> {
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
    let (named_order, named_indices) = collect_named(&scoped, &allowed_def_types, body_refs.as_ref(), nulldef_count);

    let env = ScratchEnv::new(named_indices, names);
    let nulldefs = build_nulldefs(config.nulldef_entries, &corpus.symbols, &env);
    let ctx = BuildCtx {
        symbols: &corpus.symbols,
        env: &env,
        code_files: &corpus.code_files,
        def_to_file_id: &corpus.def_to_file_id,
        def_spans: &corpus.def_spans,
        defs_by_name: &defs_by_name,
        nulldefs: &nulldefs,
    };

    eprintln!("    lowering {} named definitions...", named_order.len());
    let mut entries: Vec<Built> = Vec::new();
    let n_ok = emit_nulldef_and_named(
        &mut entries,
        config.nulldef_entries,
        &named_order,
        &ctx,
    )?;

    let named_base = nulldef_count as usize;
    let (sub_ok, sub_fail, unique_subdefs) = if config.has_subdefs {
        build_subdefs(
            &named_order,
            named_base,
            &ctx,
            &mut entries,
        )?
    } else {
        (0, 0, 0)
    };

    let entry_count = assemble_and_write(
        entries,
        &config.binary_header,
        &out_dir.join(config.out_filename),
        config.label,
    )?;
    if config.has_subdefs {
        eprintln!(
            "  {}: {n_ok} lowered, sub-defs: {sub_ok} ok/{sub_fail} fail/{unique_subdefs} unique, {entry_count} entries",
            config.label,
        );
    } else {
        eprintln!("  {}: {n_ok} lowered, {entry_count} entries", config.label);
    }
    Ok(entry_count)
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Top-level entry point
// ═══════════════════════════════════════════════════════════════════════════════

pub fn build_all(source: &Path, out_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(out_dir).map_err(|e| format!("create out dir: {e}"))?;

    let started = Instant::now();

    let corpus = parse_corpus(source)?;

    eprintln!("  compiling...");
    let builder_cell = RefCell::new(NamesBuilder::new());
    let game_count      = build_one_bin(&corpus, &GAME_CONFIG,      &builder_cell, out_dir)?;
    let frontend_count  = build_one_bin(&corpus, &FRONTEND_CONFIG,  &builder_cell, out_dir)?;
    let script_count    = build_one_bin(&corpus, &SCRIPT_CONFIG,    &builder_cell, out_dir)?;

    let names = builder_cell.into_inner().finalize(NAMES_HEADER_BYTES);
    let names_bytes = names.to_bytes();
    std::fs::write(out_dir.join("names.bin"), &names_bytes)
        .map_err(|e| format!("write names.bin: {e}"))?;

    let elapsed = started.elapsed();
    eprintln!(
        "  finished in {:.1}s — game.bin: {game_count} entries, frontend.bin: {frontend_count} entries, script.bin: {script_count} entries",
        elapsed.as_secs_f64()
    );
    Ok(())
}
