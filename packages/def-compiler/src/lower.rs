//! Lowering: convert parsed text [`Definition`]s into the typed binary def
//! structs from `defs`.
//! Each `lower_*` function drives a [`DefReader`] over a definition body and
//! produces the corresponding def struct. [`lower_def`] dispatches by def name
//! (using the same name strings as the binary `DefBody::parse`).
//! Defs that only set a subset of their fields (`UI`, `CONTROL_SCHEME`,
//! `UI_MISC_THINGS_DEF`) lower by starting from a *base* struct — the type's
//! default-constructed value (the `NULLDEF_*` entry of a compiled bin) — and
//! overriding the fields present in the text. Specialization (`specialises`)
//! flattens to the same mechanism: the ancestor chain's statements are
//! concatenated parent-first ([`flatten_specialization`]) so later statements
//! override earlier ones, and list-building method calls accumulate in order.

use crate::reader::{Args, DefReader, DefReaderError, EvalError, Evaluator};
use defs::{
    crc32,
    def::{
        binary::DefBody,
        visit::{
            DefDefault, FieldRef, FieldVisitor, MapSlot, StructSlot, VariantSlot, VecSlot,
            VisitFields,
        },
        wire::{DefIndex, DefString, PString, VecMap, WStr},
        ControlsDef, FrontEndDef, MeshRef, UiDef, UiMiscThingsDef,
        ActionInputControl, AnimatingObjectDef, AnimationEntry, AnimationEntryComponentsEntry,
        AnimationSetAnimsEntry, AppearanceDef, AppearanceModifierDef, CameraManagerDef,
        CombatAbilityBlockDefBase, CreatureDef,
        DegradableDef, DegradableInfo, EntitySoundDef,
        ExpressionSetExpressionsEntry, FlammableDef, HeroExperienceDef, HeroMorphDef, LookDef,
        MapPathEntry, ObjectAugmentationType, OpinionDeedEffectsDef, OpinionPersonalityDef,
        OpinionPersonalityTraitsPtr, OpinionReactionManagerDef, OpinionSourceDef,
        OpinionTransientOffset, OpinionTransientOffsetList, ParticleMorphsMorphsEntry,
        PlayerGuiDef, RandomAppearanceMorph, RandomAppearanceMorphBodyParts0MeshesEntry,
        RandomAppearanceMorphBodyParts0MeshesEntryTextureMorphsEntriesEntry,
        RandomAppearanceMorphBodyParts1MeshesEntry,
        RandomAppearanceMorphBodyParts1MeshesEntryTextureMorphsEntriesEntry,
        RandomAppearanceMorphBodyParts2MeshesEntry,
        RandomAppearanceMorphBodyParts2MeshesEntryTextureMorphsEntriesEntry,
        ReactionFrequencyTraitsArray, ReactionFrequencyTraitsArrayTraitsEntry,
        ReactionMatchList, ReactionMatchListElementsEntry, ReplaceableMeshDef,
        ReplaceableMeshesEntry, SoundMapF0Entry, SoundMapF0EntryValue, SoundMapF1Entry,
        SpecialEffectsDef, TextureMorphsMorphsEntry, ThingBaseDef, ThingBuildingDef,
        ThingComponentSet, ThingComponentSetEntriesEntry, ThingCreatureDef, ThingHolySiteDef,
        ThingMarkerDef, ThingNoiseDef, ThingObjectDef, ThingPhysicalSwitchDef,
        ThingSwitchDef, ThingVillageDef, UiStateDef,
        ScriptDef, WeaponDef, WeaponTrailGraphicSet, WillResponseDef,
        ControllerType, GameAction, InputKey, MouseButtonControl,
        Sprite2dFlags, TableExpansion, XboxControllerButton,
        Definition, Expr, PathSegment, Span, Spanned, Statement,
        text::SymbolTable,
    },
};
use std::collections::HashMap;


fn spanned_expr(value: Expr) -> Spanned<Expr> {
    Spanned {
        span: Span { start: 0, end: 0 },
        value,
    }
}

/// Convert a raw `i32` to a def-enum variant, returning an error for values
/// not in the enum table (strict, matching the generic lowering path).
fn from_i32_strict<T: defs::def::enums::DefEnum>(value: i32) -> Result<T, LowerError> {
    T::from_i32(value).ok_or_else(|| {
        LowerError::Unsupported(format!("invalid {} value: {}", std::any::type_name::<T>(), value))
    })
}

#[derive(Debug)]
pub enum LowerError {
    Unsupported(String),
    MissingParent { def: String, parent: String, span: Option<Span> },
    SpecializationCycle(String, Option<Span>),
    UnresolvedReference(String, Option<Span>),
    Reader(DefReaderError),
}

impl std::fmt::Display for LowerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LowerError::Unsupported(t) => write!(f, "no lowering for def type {t}"),
            LowerError::MissingParent { def, parent, .. } => {
                write!(f, "definition {def} specializes unknown parent {parent}")
            }
            LowerError::SpecializationCycle(name, _) => {
                write!(f, "specialization cycle detected at {name}")
            }
            LowerError::UnresolvedReference(r, _) => {
                write!(f, "unresolved reference: {r}")
            }
            LowerError::Reader(e) => write!(f, "{e}"),
        }
    }
}

impl From<DefReaderError> for LowerError {
    fn from(error: DefReaderError) -> Self {
        LowerError::Reader(error)
    }
}

impl LowerError {
    /// The primary source span for this error, if any — the expression,
    /// statement, or `specialises` clause that caused it. Used by diagnostic
    /// rendering.
    pub fn primary_span(&self) -> Option<Span> {
        match self {
            LowerError::UnresolvedReference(_, span) => *span,
            LowerError::Reader(DefReaderError::Eval(_, span)) => Some(*span),
            LowerError::Reader(DefReaderError::Semantic(_, span)) => *span,
            LowerError::Reader(DefReaderError::MissingField(_, span)) => Some(*span),
            LowerError::Reader(DefReaderError::MissingArg(_, span)) => Some(*span),
            LowerError::MissingParent { span, .. } => *span,
            LowerError::SpecializationCycle(_, span) => *span,
            _ => None,
        }
    }
}

// ── Environment ──────────────────────────────────────────────────────────────

/// Link-time resolution required by some lowerings.
///
/// Two def field kinds can't be resolved from header symbols alone:
/// - def references (`Children[0] SOME_DEF;`) lower to the referenced def's
///   global entry index in the compiled bin;
/// - `CDefString` fields (`Font "ENG_ARIAL_24";`) lower to the string's offset
///   in the compiled name table (names.bin).
///
/// Index/offset assignment is a whole-bin concern (the "link" step), so
/// lowering receives it as an environment.
pub trait LowerEnv {
    fn def_index(&self, name: &str) -> Option<u32>;
    fn def_string_offset(&self, string: &str) -> Option<u32>;
}

/// Environment for def types with no link-time references.
pub struct NoEnv;

impl LowerEnv for NoEnv {
    fn def_index(&self, _name: &str) -> Option<u32> {
        None
    }
    fn def_string_offset(&self, _string: &str) -> Option<u32> {
        None
    }
}

// ── Specialization ───────────────────────────────────────────────────────────

/// Flatten a definition's `specialises` chain into a single statement list,
/// most-distant ancestor first. With the reader's last-wins override semantics
/// this reproduces the game compiler's copy-parent-then-apply behaviour.
pub fn flatten_specialization<'a>(
    def: &'a Definition,
    defs_by_name: &'a HashMap<&str, &Definition>,
) -> Result<Vec<Spanned<Statement>>, LowerError> {
    let mut chain: Vec<&Definition> = vec![def];
    let mut current = def;
    while let Some(parent_name) = &current.specializes {
        let parent = *defs_by_name
            .get(parent_name.as_str())
            .ok_or_else(|| LowerError::MissingParent {
                def: def.name.clone(),
                parent: parent_name.clone(),
                span: current.specializes_span,
            })?;
        if chain.iter().any(|d| std::ptr::eq(*d, parent)) {
            return Err(LowerError::SpecializationCycle(
                def.name.clone(),
                current.specializes_span,
            ));
        }
        chain.push(parent);
        current = parent;
    }
    Ok(chain
        .iter()
        .rev()
        .flat_map(|d| d.body.iter().cloned())
        .collect())
}

/// Partition a body by extracting all statements that are method calls on a
/// specific field with a specific method name. Returns (matched, rest).
fn partition_method_calls<'a>(
    body: &'a [Spanned<Statement>],
    field: &str,
    method: &str,
) -> (Vec<&'a Spanned<Statement>>, Vec<Spanned<Statement>>) {
    let mut matched = Vec::new();
    let mut rest = Vec::new();
    for stmt in body.iter() {
        if let Statement::MethodCall(mc) = &stmt.value
            && mc.object.segments.len() == 1
            && matches!(&mc.object.segments[0], PathSegment::Field(n) if n == field)
            && mc.call.name == method
        {
            matched.push(stmt);
        } else {
            rest.push(stmt.clone());
        }
    }
    (matched, rest)
}

/// Like [`partition_method_calls`] but matches any method name on the given field.
fn partition_field_calls<'a>(
    body: &'a [Spanned<Statement>],
    field: &str,
) -> (Vec<&'a Spanned<Statement>>, Vec<Spanned<Statement>>) {
    let mut matched = Vec::new();
    let mut rest = Vec::new();
    for stmt in body.iter() {
        if let Statement::MethodCall(mc) = &stmt.value
            && mc.object.segments.len() == 1
            && matches!(&mc.object.segments[0], PathSegment::Field(n) if n == field)
        {
            matched.push(stmt);
        } else {
            rest.push(stmt.clone());
        }
    }
    (matched, rest)
}

/// Filter out statements for a specific field (any method or path) and return
/// the rest. For simple cases where a field shouldn't reach generic lowering.
fn filter_out_field(body: &[Spanned<Statement>], field: &str) -> Vec<Spanned<Statement>> {
    body.iter().filter(|stmt| {
        let segs = match &stmt.value {
            Statement::MethodCall(mc) => &mc.object.segments,
            Statement::Field(f) => &f.path.segments,
            _ => return true,
        };
        !matches!(segs.first(), Some(PathSegment::Field(n)) if n == field)
    }).cloned().collect()
}

// ── Dispatch ─────────────────────────────────────────────────────────────────



// ── CONTROL_SCHEME ───────────────────────────────────────────────────────────

/// Lower a `CONTROL_SCHEME` definition.
///
/// The text form builds the control list with
/// `Controls.Add(CActionInputControl(action, controller, key[, C2DCoordF(x, y)]))`
/// method calls, and sets the toggle booleans as plain fields. Which slot the
/// third constructor argument fills depends on the controller type, matching
/// `CPersistTraits<CActionInputControl>::TransferIn` (decomp
/// `fablelib/defs/controls_def.cpp`): `CONTROLLER_XBOX_PAD` (1) → `XboxButton`,
/// `CONTROLLER_KEYBOARD` (2) → `KeyboardKey`, `CONTROLLER_MOUSE` (3) →
/// `MouseButton`; the unused slots stay 0.
pub fn lower_controls(
    base: &ControlsDef,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
) -> Result<ControlsDef, LowerError> {
    let mut out = base.clone();
    let mut r = DefReader::new(body, symbols);

    for add in r.calls("Controls", "Add") {
        let ctor = add.ctor(0, "CActionInputControl")?;
        out.controls.push(lower_action_input_control(&ctor)?);
    }

    if let Some(v) = r.opt_bool("ToggleZTarget")? {
        out.toggle_z_target = v;
    }
    if let Some(v) = r.opt_bool("ToggleSpells")? {
        out.toggle_spells = v;
    }
    if let Some(v) = r.opt_bool("ToggleSneak")? {
        out.toggle_sneak = v;
    }
    if let Some(v) = r.opt_bool("ToggleExpressionMenu")? {
        out.toggle_expression_menu = v;
    }
    if let Some(v) = r.opt_bool("ToggleExpressionShift")? {
        out.toggle_expression_shift = v;
    }
    if let Some(v) = r.opt_bool("FlourishNeedsAttackButtonHeld")? {
        out.flourish_needs_attack_button_held = v;
    }

    r.finish()?;

    Ok(out)
}

const CONTROLLER_XBOX_PAD: i32 = 1;
const CONTROLLER_KEYBOARD: i32 = 2;
const CONTROLLER_MOUSE: i32 = 3;

fn lower_action_input_control(ctor: &Args) -> Result<ActionInputControl, LowerError> {
    let game_action = ctor.i32(0)?;
    let controller_type = ctor.i32(1)?;
    let key = ctor.i32(2)?;

    let (keyboard_key, xbox_button, mouse_button) = match controller_type {
        CONTROLLER_XBOX_PAD => (0, key, 0),
        CONTROLLER_KEYBOARD => (key, 0, 0),
        CONTROLLER_MOUSE => (0, 0, key),
        _ => {
            return Err(LowerError::Reader(DefReaderError::Semantic(
                "CActionInputControl: unknown controller type",
                None,
            )));
        }
    };

    let control_direction = match ctor.opt(3) {
        Some(_) => {
            let coord = ctor.ctor(3, "C2DCoordF")?;
            [coord.f32(0)?, coord.f32(1)?]
        }
        None => [0.0, 0.0],
    };

    Ok(ActionInputControl {
        game_action: from_i32_strict::<GameAction>(game_action)?,
        controller_type: from_i32_strict::<ControllerType>(controller_type)?,
        keyboard_key: from_i32_strict::<InputKey>(keyboard_key)?,
        xbox_button: from_i32_strict::<XboxControllerButton>(xbox_button)?,
        mouse_button: from_i32_strict::<MouseButtonControl>(mouse_button)?,
        control_direction,
    })
}

pub fn lower_front_end(
    base: &FrontEndDef,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
) -> Result<FrontEndDef, DefReaderError> {
    let mut out = base.clone();
    let mut r = DefReader::new(body, symbols);

    // The movie list is built with `vAttractModeMovie.Add("...")` calls (and
    // supports the indexed `vAttractModeMovie[i]` form).
    for add in r.calls("vAttractModeMovie", "Add") {
        out.v_attract_mode_movie.push(add.string(0)?);
    }
    for (idx, mut group) in r.indexed_sparse("vAttractModeMovie")? {
        let value = group.any_string()?;
        group.finish()?;
        if out.v_attract_mode_movie.len() <= idx {
            out.v_attract_mode_movie.resize(idx + 1, String::new());
        }
        out.v_attract_mode_movie[idx] = value;
    }

    if let Some(v) = r.opt_i32("ErrorMessageBackgroundGraphic")? { out.error_message_background_graphic = v; }
    if let Some(v) = r.opt_i32("ButtonABigGraphic")? { out.button_a_big_graphic = v; }
    if let Some(v) = r.opt_i32("ButtonBBigGraphic")? { out.button_b_big_graphic = v; }

    r.finish()?;

    Ok(out)
}

// ── UI_MISC_THINGS_DEF ───────────────────────────────────────────────────────

/// Lower a `UI_MISC_THINGS_DEF` definition onto `base` (the type's
/// default-constructed def, i.e. the `NULLDEF_UI_MISC_THINGS_DEF` entry).
/// Fields are literals or symbol-resolved ids; no link-time def resolution is
/// needed (MapPaths values are texture symbols, not def refs).
pub fn lower_ui_misc_things(
    base: &UiMiscThingsDef,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
    _env: &dyn LowerEnv,
) -> Result<UiMiscThingsDef, LowerError> {
    let mut out = base.clone();
    let mut r = DefReader::new(body, symbols);
    let eval = Evaluator::new(symbols);

    for (key, mut group) in r.keyed("MiniMapGraphics") {
        let key = eval.eval_string(key)?;
        let value = group.any_i32()?;
        group.finish()?;
        match out.mini_map_graphics.0.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = value,
            None => out.mini_map_graphics.0.push((key, value)),
        }
    }
    // MapPaths is a `std::map<u32, u32>` (decomp ui_def.cpp): the `[key]` is a
    // path id, the value a texture-symbol id (e.g. `UI_MAP_PATH_… = 5388` from
    // textures.h). Store one entry per key, last-wins, sorted by key (std::map
    // order). The bracket is NOT a dense array index — treating it as one blew
    // the entry past the chunk size.
    for (key, mut group) in r.keyed("MapPaths") {
        let id = eval.eval_u32(key)?;
        let graphic = group.any_i32()?;
        group.finish()?;
        match out.map_paths.iter_mut().find(|e| e.id == id) {
            Some(e) => e.graphic = graphic,
            None => out.map_paths.push(MapPathEntry { id, graphic }),
        }
    }
    out.map_paths.sort_by_key(|e| e.id);

    // WStr fields
    if let Some(v) = r.opt_string("SpaceSeparator")? { out.space_separator = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("CommaSeparator")? { out.comma_separator = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("NewLineSeparator")? { out.new_line_separator = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("OpenBracket")? { out.open_bracket = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("CloseBracket")? { out.close_bracket = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("Positive")? { out.positive = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("ColonSeparator")? { out.colon_separator = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("WeaponValueString")? { out.weapon_value_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("WeaponAugString")? { out.weapon_aug_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("WeaponAugNone")? { out.weapon_aug_none = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("WeaponWeightString")? { out.weapon_weight_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("WeaponLightString")? { out.weapon_light_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("WeaponHeavyString")? { out.weapon_heavy_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("WeaponKillsString")? { out.weapon_kills_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("WeaponCatMeleeString")? { out.weapon_cat_melee_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("WeaponCatRangedString")? { out.weapon_cat_ranged_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("WeaponDamageString")? { out.weapon_damage_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("TradeCostString")? { out.trade_cost_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("TradeProfitString")? { out.trade_profit_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("TradeLossString")? { out.trade_loss_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("TradeAlreadyOwnsString")? { out.trade_already_owns_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("TradeNumberInStockString")? { out.trade_number_in_stock_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("TradeDeliveryString")? { out.trade_delivery_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("TradeNoDeliveryString")? { out.trade_no_delivery_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("TradeDaysString")? { out.trade_days_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("TradeBuyString")? { out.trade_buy_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("TradeSellString")? { out.trade_sell_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("TradeWantedString")? { out.trade_wanted_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("QuestFailedString")? { out.quest_failed_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("FailedString")? { out.failed_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("SucceededString")? { out.succeeded_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("Plus")? { out.plus = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("Minus")? { out.minus = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("ObjectsRewardString")? { out.objects_reward_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("NoneString")? { out.none_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("CheckGuildString")? { out.check_guild_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("QuestStartingString")? { out.quest_starting_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("YouString")? { out.you_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("OwnString")? { out.own_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("NoString")? { out.no_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("HousesString")? { out.houses_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("HouseString")? { out.house_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("InString")? { out.in_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("ShopsString")? { out.shops_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("ShopString")? { out.shop_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("ThereString")? { out.there_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("AreString")? { out.are_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("IsString")? { out.is_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("ForString")? { out.for_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("SaleString")? { out.sale_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("GeneralString")? { out.general_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("TatooString")? { out.tatoo_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("BarberString")? { out.barber_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("TitleString")? { out.title_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("LevelString")? { out.level_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("LogBookBasicsCategoryString")? { out.log_book_basics_category_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("LogBookObjectsCategoryString")? { out.log_book_objects_category_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("LogBookTownsCategoryString")? { out.log_book_towns_category_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("LogBookHeroCategoryString")? { out.log_book_hero_category_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("LogBookCombatCategoryString")? { out.log_book_combat_category_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("LogBookQuestCategoryString")? { out.log_book_quest_category_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("LogBookStoryCategoryString")? { out.log_book_story_category_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("LogBookBasicsCategoryNameString")? { out.log_book_basics_category_name_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("LogBookObjectsCategoryNameString")? { out.log_book_objects_category_name_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("LogBookTownsCategoryNameString")? { out.log_book_towns_category_name_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("LogBookHeroCategoryNameString")? { out.log_book_hero_category_name_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("LogBookCombatCategoryNameString")? { out.log_book_combat_category_name_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("LogBookQuestCategoryNameString")? { out.log_book_quest_category_name_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("LogBookStoryCategoryNameString")? { out.log_book_story_category_name_string = WStr::from(v.as_str()); }
    if let Some(v) = r.opt_string("FrontEndMusic")? { out.front_end_music = WStr::from(v.as_str()); }

    // String fields (sound names)
    if let Some(v) = r.opt_string("SoundUpDown")? { out.sound_up_down = v; }
    if let Some(v) = r.opt_string("SoundSlider")? { out.sound_slider = v; }
    if let Some(v) = r.opt_string("SoundBack")? { out.sound_back = v; }
    if let Some(v) = r.opt_string("SoundForward")? { out.sound_forward = v; }
    if let Some(v) = r.opt_string("SoundError")? { out.sound_error = v; }
    if let Some(v) = r.opt_string("SoundExit")? { out.sound_exit = v; }
    if let Some(v) = r.opt_string("CountUpSound")? { out.count_up_sound = v; }
    if let Some(v) = r.opt_string("SoundKeyboardUp")? { out.sound_keyboard_up = v; }
    if let Some(v) = r.opt_string("SoundKeyboardDown")? { out.sound_keyboard_down = v; }
    if let Some(v) = r.opt_string("SoundKeyboardLeft")? { out.sound_keyboard_left = v; }
    if let Some(v) = r.opt_string("SoundKeyboardRight")? { out.sound_keyboard_right = v; }
    if let Some(v) = r.opt_string("SoundKeyboardEnterCharacter")? { out.sound_keyboard_enter_character = v; }
    if let Some(v) = r.opt_string("SoundKeyboardDeleteCharacter")? { out.sound_keyboard_delete_character = v; }
    if let Some(v) = r.opt_string("SoundKeyboardDone")? { out.sound_keyboard_done = v; }

    // u32 fields
    if let Some(v) = r.opt_u32("CoreGraphic")? { out.core_graphic = v; }
    if let Some(v) = r.opt_u32("VignetteGraphic")? { out.vignette_graphic = v; }
    if let Some(v) = r.opt_u32("OptionalGraphic")? { out.optional_graphic = v; }
    if let Some(v) = r.opt_u32("FeatGraphic")? { out.feat_graphic = v; }
    if let Some(v) = r.opt_u32("TotalSpellsInPalettes")? { out.total_spells_in_palettes = v; }
    if let Some(v) = r.opt_u32("TotalSpellsInContainer")? { out.total_spells_in_container = v; }
    if let Some(v) = r.opt_u32("TotalAssignableThings")? { out.total_assignable_things = v; }
    if let Some(v) = r.opt_u32("QuestStartScreenMusic")? { out.quest_start_screen_music = v; }
    if let Some(v) = r.opt_u32("QuestCompleteScreenMusic")? { out.quest_complete_screen_music = v; }
    if let Some(v) = r.opt_u32("QuestFailureScreenMusic")? { out.quest_failure_screen_music = v; }
    if let Some(v) = r.opt_u32("DeathScreenMusic")? { out.death_screen_music = v; }
    if let Some(v) = r.opt_u32("SaveHeroGraphicIndex")? { out.save_hero_graphic_index = v; }
    if let Some(v) = r.opt_u32("KeyboardSmallKeyGraphic")? { out.keyboard_small_key_graphic = v; }
    if let Some(v) = r.opt_u32("KeyboardLargeKeyGraphic")? { out.keyboard_large_key_graphic = v; }

    // f32 fields
    if let Some(v) = r.opt_f32("RingCenterX")? { out.ring_center_x = v; }
    if let Some(v) = r.opt_f32("RingCenterY")? { out.ring_center_y = v; }
    if let Some(v) = r.opt_f32("PCRingCenterX")? { out.pc_ring_center_x = v; }
    if let Some(v) = r.opt_f32("PCRingCenterY")? { out.pc_ring_center_y = v; }
    if let Some(v) = r.opt_f32("WorldMapOffsetX")? { out.world_map_offset_x = v; }
    if let Some(v) = r.opt_f32("WorldMapOffsetY")? { out.world_map_offset_y = v; }
    if let Some(v) = r.opt_f32("WorldMapWidth")? { out.world_map_width = v; }
    if let Some(v) = r.opt_f32("WorldMapHeight")? { out.world_map_height = v; }
    if let Some(v) = r.opt_f32("HeroDollTLX")? { out.hero_doll_tlx = v; }
    if let Some(v) = r.opt_f32("HeroDollTLY")? { out.hero_doll_tly = v; }
    if let Some(v) = r.opt_f32("HeroDollBRX")? { out.hero_doll_brx = v; }
    if let Some(v) = r.opt_f32("HeroDollBRY")? { out.hero_doll_bry = v; }
    if let Some(v) = r.opt_f32("HeroDollSphereRadius")? { out.hero_doll_sphere_radius = v; }
    if let Some(v) = r.opt_f32("HeroDollTLX_PC")? { out.hero_doll_tlx_pc = v; }
    if let Some(v) = r.opt_f32("HeroDollTLY_PC")? { out.hero_doll_tly_pc = v; }
    if let Some(v) = r.opt_f32("HeroDollBRX_PC")? { out.hero_doll_brx_pc = v; }
    if let Some(v) = r.opt_f32("HeroDollBRY_PC")? { out.hero_doll_bry_pc = v; }
    if let Some(v) = r.opt_f32("HeroDollSphereRadius_PC")? { out.hero_doll_sphere_radius_pc = v; }
    if let Some(v) = r.opt_f32("HeroDollFrameTLX_PC")? { out.hero_doll_frame_tlx_pc = v; }
    if let Some(v) = r.opt_f32("HeroDollFrameTLY_PC")? { out.hero_doll_frame_tly_pc = v; }
    if let Some(v) = r.opt_f32("HeroDollFrameEmulateListOffset")? { out.hero_doll_frame_emulate_list_offset = v; }
    if let Some(v) = r.opt_f32("DigitCountTime")? { out.digit_count_time = v; }
    if let Some(v) = r.opt_f32("TimeInSecsForFade")? { out.time_in_secs_for_fade = v; }
    if let Some(v) = r.opt_f32("BackBufferFilterSaturation")? { out.back_buffer_filter_saturation = v; }
    if let Some(v) = r.opt_f32("BackBufferFilterContrast")? { out.back_buffer_filter_contrast = v; }
    if let Some(v) = r.opt_f32("BackBufferFilterBrightness")? { out.back_buffer_filter_brightness = v; }
    if let Some(v) = r.opt_f32("BackBufferFilterTintR")? { out.back_buffer_filter_tint_r = v; }
    if let Some(v) = r.opt_f32("BackBufferFilterTintG")? { out.back_buffer_filter_tint_g = v; }
    if let Some(v) = r.opt_f32("BackBufferFilterTintB")? { out.back_buffer_filter_tint_b = v; }
    if let Some(v) = r.opt_f32("BackBufferFilterTintScale")? { out.back_buffer_filter_tint_scale = v; }
    if let Some(v) = r.opt_f32("BackBufferDiffuseScale")? { out.back_buffer_diffuse_scale = v; }
    if let Some(v) = r.opt_f32("BackBufferAmbientScale")? { out.back_buffer_ambient_scale = v; }
    if let Some(v) = r.opt_f32("MinimumFilterColor")? { out.minimum_filter_color = v; }

    r.finish()?;

    Ok(out)
}

/// Newly-created [`UiStateDef`] entries default-fill with the game compiler's
/// initial values (zoom 1.0), not zero.
fn default_ui_state() -> UiStateDef {
    UiStateDef {
        colour_r: 1.0,
        colour_g: 1.0,
        colour_b: 1.0,
        colour_a: 1.0,
        zoom_x: 1.0,
        zoom_y: 1.0,
        update_time: -1.0,
        state_change_flag: 7,
        ..UiStateDef::default()
    }
}

// ── UI ───────────────────────────────────────────────────────────────────────

/// Resolve a reference-or-scalar value: integers, header symbols, and `|`/`+`
/// expressions evaluate directly; a bare symbol that isn't in the symbol table
/// is treated as a def reference and resolved to its entry index via `env`.
pub(crate) fn resolve_ref(
    eval: &Evaluator,
    env: &dyn LowerEnv,
    expr: &Spanned<Expr>,
) -> Result<u32, LowerError> {
    let span = expr.span;
    // A quoted def name (`DummyObject "OBJECT_…";`) is a def reference just like
    // the bare-symbol form; resolve it to the referenced def's index.
    if let Expr::String(name) = &expr.value {
        return env
            .def_index(name)
            .ok_or_else(|| LowerError::UnresolvedReference(name.clone(), Some(span)));
    }
    // A bare `NULL` reference already evaluates to 0 in the `Evaluator`, so it
    // never reaches the `UnknownSymbol` arm here.
    match eval.u32(expr) {
        Ok(v) => Ok(v),
        Err(EvalError::UnknownSymbol(name)) => env
            .def_index(&name)
            .ok_or(LowerError::UnresolvedReference(name, Some(span))),
        Err(e) => Err(LowerError::Reader(DefReaderError::Eval(e, span))),
    }
}

/// Like [`resolve_ref`] but for signed fields: integer literals (including
/// negatives) evaluate directly, while an unknown symbol is a def reference.
pub(crate) fn resolve_ref_i32(
    eval: &Evaluator,
    env: &dyn LowerEnv,
    expr: &Spanned<Expr>,
) -> Result<i32, LowerError> {
    let span = expr.span;
    if let Expr::String(name) = &expr.value {
        // An empty def-reference (`ReplacementObject ""`) is the null-def
        // sentinel → index 0 (verified vs retail CSmashableDef).
        if name.is_empty() {
            return Ok(0);
        }
        return env
            .def_index(name)
            .map(|v| v as i32)
            .ok_or_else(|| LowerError::UnresolvedReference(name.clone(), Some(span)));
    }
    match eval.i32(expr) {
        Ok(v) => Ok(v),
        Err(EvalError::UnknownSymbol(name)) => env
            .def_index(&name)
            .map(|v| v as i32)
            .ok_or(LowerError::UnresolvedReference(name, Some(span))),
        Err(e) => Err(LowerError::Reader(DefReaderError::Eval(e, span))),
    }
}

/// Set `vec[idx] = value`, growing the vector with `fill` as needed.
fn set_grow<T: Clone>(vec: &mut Vec<T>, idx: usize, value: T, fill: T) {
    if vec.len() <= idx {
        vec.resize(idx + 1, fill);
    }
    vec[idx] = value;
}

/// Lower a `UI` definition onto `base` (the type's default-constructed def,
/// i.e. the `NULLDEF_UI` entry). `Children`/`Sprites` values reference other
/// defs by name and `Font` is a `CDefString`; both resolve through `env`.
pub fn lower_ui(
    base: &UiDef,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
    env: &dyn LowerEnv,
) -> Result<UiDef, LowerError> {
    let mut out = base.clone();
    let mut r = DefReader::new(body, symbols);
    let eval = Evaluator::new(symbols);

    // The NULLDEF_UI base has zeroed state defaults; the game compiler
    // initializes state_change_flag to 7 (update movement|colour|zoom).
    // Apply this *before* text processing so explicit text values can
    // override it.
    for state in &mut out.states {
        if state.state_change_flag == 0 {
            state.state_change_flag = 7;
        }
    }

    // The game compiler default-initializes `Font` to "ENG_ARIAL_16" via
    // `CUIElementDef`'s `SetDefaultString` (retail NULLDEF_UI Font = offset 224).
    // Seed it before text so an explicit/inherited `Font` statement overrides it,
    // and entries that never set Font inherit the default rather than a null (0).
    out.font = DefString(
        env.def_string_offset("ENG_ARIAL_16")
            .expect("intern ENG_ARIAL_16") as i32,
    );

    if let Some(v) = r.opt_i32("Type")? { out.type_ = from_i32_strict(v)?; }
    // MeshIndex is usually a graphics-bank symbol (`MESH_*`, a header constant),
    // but list widgets reference another UI element by def-name
    // (`MeshIndex PC_LIST_SCROLL_BAR`), which resolves via the def index.
    if let Some(expr) = r.opt_expr("MeshIndex") {
        out.mesh_index = MeshRef::from_index(out.type_.to_i32(), resolve_ref(&eval, env, expr)? as i32);
    }
    if let Some(v) = r.opt_string("TextValue")? { out.text_value = WStr(v); }
    if let Some(v) = r.opt_f32("Height")? { out.height = v; }
    if let Some(v) = r.opt_f32("Width")? { out.width = v; }
    if let Some(v) = r.opt_i32("ExpansionType")? { out.expansion_type = TableExpansion(v); }
    if let Some(v) = r.opt_bool("TextLineBreak")? { out.text_line_break = v; }
    if let Some(v) = r.opt_bool("ScaleText")? { out.scale_text = v; }
    if let Some(v) = r.opt_bool("Independant")? { out.independant = v; }
    if let Some(v) = r.opt_i32("MeshType")? { out.mesh_type = from_i32_strict(v)?; }
    if let Some(v) = r.opt_f32("TextWindowTLX")? { out.text_window_tlx = v; }
    if let Some(v) = r.opt_f32("TextWindowTLY")? { out.text_window_tly = v; }
    if let Some(v) = r.opt_f32("TextWindowBRX")? { out.text_window_brx = v; }
    if let Some(v) = r.opt_f32("TextWindowBRY")? { out.text_window_bry = v; }
    if let Some(v) = r.opt_i32("Layer")? { out.layer = v; }
    if let Some(v) = r.opt_f32("Angle")? { out.angle = v; }
    if let Some(v) = r.opt_bool("PositionIsCenter")? { out.position_is_center = v; }
    if let Some(v) = r.opt_f32("ScrollingSpeed")? { out.scrolling_speed = v; }
    if let Some(v) = r.opt_bool("Wrapping")? { out.wrapping = v; }
    if let Some(v) = r.opt_bool("Inverted")? { out.inverted = v; }
    if let Some(v) = r.opt_f32("PositionOffsetX")? { out.position_offset_x = v; }
    if let Some(v) = r.opt_f32("PositionOffsetY")? { out.position_offset_y = v; }
    if let Some(v) = r.opt_u32("AlphaOffset")? { out.alpha_offset = v; }
    if let Some(v) = r.opt_f32("UpX")? { out.up_x = v; }
    if let Some(v) = r.opt_f32("UpY")? { out.up_y = v; }
    if let Some(v) = r.opt_f32("UpZ")? { out.up_z = v; }
    if let Some(v) = r.opt_f32("ForwardX")? { out.forward_x = v; }
    if let Some(v) = r.opt_f32("ForwardY")? { out.forward_y = v; }
    if let Some(v) = r.opt_f32("ForwardZ")? { out.forward_z = v; }
    if let Some(v) = r.opt_f32("RotationAxisX")? { out.rotation_axis_x = v; }
    if let Some(v) = r.opt_f32("RotationAxisY")? { out.rotation_axis_y = v; }
    if let Some(v) = r.opt_f32("RotationAxisZ")? { out.rotation_axis_z = v; }
    if let Some(v) = r.opt_f32("RotationSpeed")? { out.rotation_speed = v; }
    // Like MeshIndex, AnimationIndex can name another UI element def
    // (`AnimationIndex PC_LIST_SCROLL_BAR_OUTSIDE`).
    if let Some(expr) = r.opt_expr("AnimationIndex") { out.animation_index = MeshRef::from_index(out.type_.to_i32(), resolve_ref(&eval, env, expr)? as i32); }
    if let Some(expr) = r.opt_expr("DownArrow") {
        out.down_arrow = DefIndex(resolve_ref_i32(&eval, env, expr)?);
    }
    if let Some(expr) = r.opt_expr("UpArrow") {
        out.up_arrow = DefIndex(resolve_ref_i32(&eval, env, expr)?);
    }
    if let Some(v) = r.opt_i32("UpLimit")? { out.up_limit = v; }
    if let Some(v) = r.opt_i32("DownLimit")? { out.down_limit = v; }
    if let Some(v) = r.opt_bool("Scrolling")? { out.scrolling = v; }
    if let Some(v) = r.opt_bool("ComputeOffsetsOnActivate")? { out.compute_offsets_on_activate = v; }
    if let Some(v) = r.opt_f32("MinX")? { out.min_x = v; }
    if let Some(v) = r.opt_f32("MinY")? { out.min_y = v; }
    if let Some(v) = r.opt_f32("MaxX")? { out.max_x = v; }
    if let Some(v) = r.opt_f32("MaxY")? { out.max_y = v; }
    if let Some(v) = r.opt_f32("StepX")? { out.step_x = v; }
    if let Some(v) = r.opt_f32("StepY")? { out.step_y = v; }
    if let Some(v) = r.opt_f32("DimensionsX")? { out.dimensions_x = v; }
    if let Some(v) = r.opt_f32("DimensionsY")? { out.dimensions_y = v; }
    if let Some(expr) = r.opt_expr("SliderLeft") {
        out.slider_left = DefIndex(resolve_ref_i32(&eval, env, expr)?);
    }
    if let Some(expr) = r.opt_expr("SliderRight") {
        out.slider_right = DefIndex(resolve_ref_i32(&eval, env, expr)?);
    }
    if let Some(v) = r.opt_i32("Action")? { out.action = from_i32_strict(v)?; }
    if let Some(v) = r.opt_i32("ActionOnBack")? { out.action_on_back = from_i32_strict(v)?; }
    if let Some(v) = r.opt_i32("ActionOnSelected")? { out.action_on_selected = from_i32_strict(v)?; }
    if let Some(v) = r.opt_i32("ActionOnUnselected")? { out.action_on_unselected = from_i32_strict(v)?; }
    if let Some(v) = r.opt_i32("ActionOnDestruction")? { out.action_on_destruction = from_i32_strict(v)?; }
    if let Some(v) = r.opt_i32("ActionOnLeftClicked")? { out.action_on_left_clicked = from_i32_strict(v)?; }
    if let Some(v) = r.opt_i32("ActionOnLeftUnclicked")? { out.action_on_left_unclicked = from_i32_strict(v)?; }
    if let Some(v) = r.opt_i32("ActionOnLeftHeld")? { out.action_on_left_held = from_i32_strict(v)?; }
    if let Some(v) = r.opt_i32("ActionOnRightClicked")? { out.action_on_right_clicked = from_i32_strict(v)?; }
    if let Some(v) = r.opt_i32("ActionOnDropped")? { out.action_on_dropped = from_i32_strict(v)?; }
    if let Some(v) = r.opt_i32("ActionOnDroppedNowhere")? { out.action_on_dropped_nowhere = from_i32_strict(v)?; }
    if let Some(v) = r.opt_i32("PreAction")? { out.pre_action = from_i32_strict(v)?; }
    if let Some(v) = r.opt_i32("ActionOnDraggedUp")? { out.action_on_dragged_up = from_i32_strict(v)?; }
    if let Some(v) = r.opt_i32("ActionOnDraggedDown")? { out.action_on_dragged_down = from_i32_strict(v)?; }
    if let Some(v) = r.opt_i32("ActionOnLeftClickedAbove")? { out.action_on_left_clicked_above = from_i32_strict(v)?; }
    if let Some(v) = r.opt_i32("ActionOnLeftClickedUnder")? { out.action_on_left_clicked_under = from_i32_strict(v)?; }
    if let Some(v) = r.opt_f32("InputDelay")? { out.input_delay = v; }
    if let Some(v) = r.opt_bool("DrawFromViewport")? { out.draw_from_viewport = v; }
    if let Some(v) = r.opt_u32("TextBankIndex")? { out.text_bank_index = v; }
    if let Some(expr) = r.opt_expr("ActionText") {
        out.action_text = DefIndex(resolve_ref_i32(&eval, env, expr)?);
    }
    if let Some(expr) = r.opt_expr("KeyText") {
        out.key_text = DefIndex(resolve_ref_i32(&eval, env, expr)?);
    }
    if let Some(expr) = r.opt_expr("Redefiner") {
        out.redefiner = DefIndex(resolve_ref_i32(&eval, env, expr)?);
    }
    if let Some(expr) = r.opt_expr("UndefinedWarning") {
        out.undefined_warning = DefIndex(resolve_ref_i32(&eval, env, expr)?);
    }
    if let Some(v) = r.opt_bool("EditBoxParentIsButton")? { out.edit_box_parent_is_button = v; }
    if let Some(v) = r.opt_bool("PasswordBox")? { out.password_box = v; }
    if let Some(v) = r.opt_i32("EditBoxCharLimit")? { out.edit_box_char_limit = v; }
    if let Some(v) = r.opt_bool("EditBoxUsesIME")? { out.edit_box_uses_ime = v; }
    if let Some(v) = r.opt_string("MovieFilename")? { out.movie_filename = WStr(v); }
    if let Some(v) = r.opt_bool("DisallowSpaceAsFirstChar")? { out.disallow_space_as_first_char = v; }
    if let Some(v) = r.opt_bool("LayerIndependant")? { out.layer_independant = v; }
    if let Some(v) = r.opt_bool("BastardChild")? { out.bastard_child = v; }
    if let Some(v) = r.opt_i32("Alignement")? { out.alignement = from_i32_strict(v)?; }
    if let Some(v) = r.opt_bool("RandomSwap")? { out.random_swap = v; }
    if let Some(v) = r.opt_bool("UseRelativeZoom")? { out.use_relative_zoom = v; }
    if let Some(v) = r.opt_bool("UseRelativePosition")? { out.use_relative_position = v; }
    if let Some(v) = r.opt_i32("HoveredState")? { out.hovered_state = v; }
    if let Some(v) = r.opt_i32("LeftClickedState")? { out.left_clicked_state = v; }
    if let Some(v) = r.opt_i32("RightClickedState")? { out.right_clicked_state = v; }
    if let Some(v) = r.opt_i32("ViewAreaTLX")? { out.view_area_tlx = v; }
    if let Some(v) = r.opt_i32("ViewAreaTLY")? { out.view_area_tly = v; }
    if let Some(v) = r.opt_i32("ViewAreaBRX")? { out.view_area_brx = v; }
    if let Some(v) = r.opt_i32("ViewAreaBRY")? { out.view_area_bry = v; }
    if let Some(v) = r.opt_bool("UseViewArea")? { out.use_view_area = v; }
    if let Some(v) = r.opt_bool("PartOfListTree")? { out.part_of_list_tree = v; }
    if let Some(v) = r.opt_bool("PCStyle")? { out.pc_style = v; }
    if let Some(v) = r.opt_i32("Sprite2DFlag")? { out.sprite2_d_flag = Sprite2dFlags(v); }

    // Children[i] <def name>
    for (idx, mut group) in r.indexed_sparse("Children")? {
        let value = resolve_ref(&eval, env, group.any_expr()?)?;
        group.finish()?;
        set_grow(&mut out.children, idx, DefIndex(value as i32), DefIndex(0));
    }

    // Font "<string>" — a CDefString: lowers to the string's name-table offset.
    if let Some(expr) = r.opt_expr("Font") {
        out.font = match &expr.value {
            Expr::String(s) => DefString(
                env.def_string_offset(s)
                    .ok_or_else(|| LowerError::UnresolvedReference(s.clone(), Some(expr.span)))?
                    as i32,
            ),
            _ => DefString(eval.eval_i32(expr)?),
        };
    }

    // Sprites[<TABLE_SPRITES_*>] <def name>
    for (key, mut group) in r.keyed("Sprites") {
        let key = eval.eval_i32(key)?;
        let value = resolve_ref(&eval, env, group.any_expr()?)? as i32;
        group.finish()?;
        out.sprites.insert(key, DefIndex(value));
    }

    // HorizontalSeparations[i] / VerticalSeparations[i]
    for (idx, mut group) in r.indexed_sparse("HorizontalSeparations")? {
        let value = group.any_u32()?;
        group.finish()?;
        set_grow(&mut out.horizontal_separations, idx, value, 0);
    }
    for (idx, mut group) in r.indexed_sparse("VerticalSeparations")? {
        let value = group.any_u32()?;
        group.finish()?;
        set_grow(&mut out.vertical_separations, idx, value, 0);
    }

    // States[i].<field> — grows the state vector, default-filling gaps.
    for (idx, mut group) in r.indexed_sparse("States")? {
        if out.states.len() <= idx {
            out.states.resize(idx + 1, default_ui_state());
        }
        apply_ui_state(&mut out.states[idx], &mut group, &eval, env)?;
        group.finish()?;
    }

    // NonScrollingChildren[i] <index>
    for (idx, mut group) in r.indexed_sparse("NonScrollingChildren")? {
        let value = resolve_ref(&eval, env, group.any_expr()?)?;
        group.finish()?;
        set_grow(&mut out.non_scrolling_children, idx, DefIndex(value as i32), DefIndex(0));
    }

    // ActionMap[<action>] "<string>"
    for (key, mut group) in r.keyed("ActionMap") {
        let key = eval.eval_u32(key)?;
        let value = group.any_string()?;
        group.finish()?;
        out.action_map.insert(key, value);
    }

    // ActionMapAliases[<action>] <action>
    for (key, mut group) in r.keyed("ActionMapAliases") {
        let key = eval.eval_u32(key)?;
        let value = group.any_u32()?;
        group.finish()?;
        out.action_map_aliases.insert(key, value);
    }

    // ActionOrder[i] <action>
    for (idx, mut group) in r.indexed_sparse("ActionOrder")? {
        let value = group.any_u32()?;
        group.finish()?;
        set_grow(&mut out.action_order, idx, value, 0);
    }

    // SwappingStates[i] / SwappingTimes[i]
    for (idx, mut group) in r.indexed_sparse("SwappingStates")? {
        let value = group.any_u32()?;
        group.finish()?;
        set_grow(&mut out.swapping_states, idx, value, 0);
    }
    for (idx, mut group) in r.indexed_sparse("SwappingTimes")? {
        let value = group.any_f32()?;
        group.finish()?;
        set_grow(&mut out.swapping_times, idx, value, 0.0);
    }

    // ShapeChildren[i] <index into Children>
    for (idx, mut group) in r.indexed_sparse("ShapeChildren")? {
        let value = resolve_ref(&eval, env, group.any_expr()?)?;
        group.finish()?;
        set_grow(&mut out.shape_children, idx, DefIndex(value as i32), DefIndex(0));
    }

    r.finish()?;

    Ok(out)
}

fn apply_ui_state(
    state: &mut UiStateDef,
    r: &mut DefReader,
    eval: &Evaluator,
    env: &dyn LowerEnv,
) -> Result<(), LowerError> {
    if let Some(expr) = r.opt_expr("GraphicIndex") {
        // GraphicIndex can be a def reference ("PARTICLE_TEST") or a header
        // constant ("UI_TABLE_TEST_TL_FE"). Try def_index first to resolve
        // symbols that are both a def name and a header define.
        state.graphic_index = match &expr.value {
            Expr::Symbol(name) => env
                .def_index(name)
                .or_else(|| eval.u32(expr).ok())
                .ok_or_else(|| LowerError::UnresolvedReference(name.clone(), Some(expr.span)))?,
            _ => resolve_ref(eval, env, expr)?,
        };
    }
    if let Some(v) = r.opt_f32("PositionX")? { state.position_x = v; }
    if let Some(v) = r.opt_f32("PositionY")? { state.position_y = v; }
    if let Some(v) = r.opt_f32("ZoomX")? { state.zoom_x = v; }
    if let Some(v) = r.opt_f32("ZoomY")? { state.zoom_y = v; }
    if let Some(v) = r.opt_f32("ColourR")? { state.colour_r = v; }
    if let Some(v) = r.opt_f32("ColourG")? { state.colour_g = v; }
    if let Some(v) = r.opt_f32("ColourB")? { state.colour_b = v; }
    if let Some(v) = r.opt_f32("ColourA")? { state.colour_a = v; }
    if let Some(v) = r.opt_f32("UpdateTime")? { state.update_time = v; }
    if let Some(v) = r.opt_i32("StateChangeType")? {
        state.state_change_type = from_i32_strict(v)?;
    }
    if let Some(v) = r.opt_bool("LinearChange")? { state.linear_change = v; }
    if let Some(v) = r.opt_u32("StateChangeFlag")? { state.state_change_flag = v; }
    for (idx, mut group) in r.indexed_sparse("ChildrenNotAffected")? {
        let value = group.any_i32()?;
        group.finish()?;
        set_grow(&mut state.children_not_affected, idx, value, 0);
    }
    Ok(())
}
// Generic, type-driven lowering: apply a def's text statements to its
// `#[derive(DefStruct)]` fields via the [`VisitFields`] walk, with the field's Rust
// type (as a [`FieldRef`]) determining how each statement form is read:
// - scalars/strings — leaf statements (`Health 100;`)
// - enums/flags — leaf statements evaluated through the symbol table
// - `DefString`/`DefIndex` — resolved through the [`LowerEnv`]
// - `Vec<T>` — `Field.Add(...)` calls and/or `Field[i] …` indexed statements
// - maps — `Field[key] value;` keyed statements
// - compounds (`WireStruct`) — nested paths (`Graphic.BankIndex …`) or
//   positional constructor calls (`C2DCoordF(x, y)`)
// - variants (`DefVariant`) — constructor calls naming the case's C++
//   class (`CPhysicalPrimitiveSphere("", 0.2)`)
// Fields absent from the text keep their (base def's) values; unconsumed
// statements are silently dropped (matching the C++ `Transfer` behaviour
// where field names that don't exist on the target type are naturally
// skipped — important for cross-type specialization).





/// Lower a `#[derive(DefStruct)]` type generically: `base` supplies defaults (the
/// type's NULLDEF), and text statements override individual fields.
/// Returns the lowered value and any unconsumed statements (misspelled field
/// names, cross-type specialization leftovers) so the caller can emit warnings.
pub fn lower_generic<T: VisitFields + Clone>(
    base: &T,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
    env: &dyn LowerEnv,
) -> Result<(T, Vec<Spanned<Statement>>), LowerError> {
    let mut out = (*base).clone();
    let stripped = strip_superseded_by_clear(body);
    let mut reader = DefReader::new(&stripped, symbols);
    {
        let mut applier = Applier { r: &mut reader, env, error: None };
        out.visit_fields(&mut applier);
        if let Some(error) = applier.error {
            return Err(error);
        }
    }
    let remaining = reader.remaining_statements();
    Ok((out, remaining))
}

/// `Field.clear()` has statement-order semantics: it discards everything added
/// to that container *before* it. The applier is field-oriented (it gathers all
/// of a field's statements at once, out of order), so honor `clear()` by
/// dropping every same-field statement that precedes the field's last `clear()`
/// call. The `clear()` itself is kept so `apply_vec_named`/`apply_map_named`
/// also wipe the NULLDEF base defaults. Critical for merged tagged blocks where
/// a parent's `Add`s precede a child's `clear()`.
fn strip_superseded_by_clear(body: &[Spanned<Statement>]) -> Vec<Spanned<Statement>> {
    fn field_name(st: &Spanned<Statement>) -> Option<&str> {
        let segs = match &st.value {
            Statement::MethodCall(mc) => &mc.object.segments,
            Statement::Field(f) => &f.path.segments,
            _ => return None,
        };
        match segs.first() {
            Some(PathSegment::Field(n)) => Some(n.as_str()),
            _ => None,
        }
    }
    let mut last_clear: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (i, st) in body.iter().enumerate() {
        if let Statement::MethodCall(mc) = &st.value
            && mc.call.name == "clear"
            && mc.object.segments.len() == 1
            && let Some(f) = field_name(st)
        {
            last_clear.insert(f, i);
        }
    }
    if last_clear.is_empty() {
        return body.to_vec();
    }
    body.iter()
        .enumerate()
        .filter(|(i, st)| match field_name(st) {
            Some(f) => last_clear.get(f).is_none_or(|&cp| *i >= cp),
            None => true,
        })
        .map(|(_, st)| st.clone())
        .collect()
}


struct Applier<'r, 'a, 's, 'e> {
    r: &'r mut DefReader<'a, 's>,
    env: &'e dyn LowerEnv,
    error: Option<LowerError>,
}

impl FieldVisitor for Applier<'_, '_, '_, '_> {
    fn field(&mut self, name: &'static str, field: FieldRef<'_>) {
        if self.error.is_none()
            && let Err(error) = self.apply_named(name, field)
        {
            self.error = Some(error);
        }
    }
}

/// Map a variant type to its ctor-name → tag table. The C++ class names that
/// select a case aren't in the wire model; the tables are built from the
/// decomp data and verified against retail binaries.
fn variant_tag_from_ctor(type_name: &str, ctor: &str) -> Option<u32> {
    match type_name {
        "PhysicalPrimitiveInitType" => match ctor {
            "CPhysicalPrimitiveNull" => Some(0),
            "CPhysicalPrimitiveSphere" => Some(1),
            "CPhysicalPrimitiveCylinder" => Some(2),
            "CPhysicalPrimitiveMesh" => Some(3),
            _ => None,
        },
        "ReactionMatchListElementsEntry" => match ctor {
            "Secondary" => Some(0),
            "SecondaryCentred" => Some(1),
            "Pyramid" => Some(2),
            "MRBlob" => Some(3),
            _ => None,
        },
        _ => None,
    }
}

impl<'a, 's> Applier<'_, 'a, 's, '_> {
    fn semantic<T>(&self, message: &'static str) -> Result<T, LowerError> {
        Err(LowerError::Reader(DefReaderError::Semantic(message, None)))
    }

    // ── named fields (one def_struct field) ─────────────────────────────────

    fn apply_named(&mut self, name: &'static str, field: FieldRef<'_>) -> Result<(), LowerError> {
        match field {
            FieldRef::F32(slot) => {
                if let Some(v) = self.r.opt_f32(name)? {
                    *slot = v;
                }
            }
            // Strict numeric: a plain i32/u32 field accepts a number, a resolved
            // constant, or a compound — NOT a string or a def reference. Reference
            // fields are typed `DefIndex` (RT). An unknown symbol here is a real
            // error (typo), not a silent def-index lookup.
            FieldRef::I32(slot) => {
                if let Some(v) = self.r.opt_i32(name)? {
                    *slot = v;
                }
            }
            FieldRef::U32(slot) => {
                if let Some(v) = self.r.opt_u32(name)? {
                    *slot = v;
                }
            }
            FieldRef::Bool(slot) => {
                if let Some(v) = self.r.opt_bool(name)? {
                    *slot = v;
                }
            }
            FieldRef::Str(slot) => {
                if let Some(v) = self.r.opt_string(name)? {
                    *slot = v;
                }
            }
            FieldRef::WStr(slot) => {
                if let Some(v) = self.r.opt_string(name)? {
                    *slot = WStr(v);
                }
            }
            FieldRef::Enum(slot) => {
                if let Some(v) = self.r.opt_i32(name)?
                    && slot.set_i32(v).is_err()
                {
                    return self.semantic("enum value out of table");
                }
            }
            FieldRef::Flags(slot) => {
                if let Some(v) = self.r.opt_i32(name)? {
                    slot.set_i32(v);
                }
            }
            FieldRef::DefString(slot) => {
                if let Some(expr) = self.r.opt_expr(name) {
                    *slot = DefString(self.eval_def_string(expr)?);
                }
            }
            FieldRef::DefIndex(slot) => {
                if let Some(expr) = self.r.opt_expr(name) {
                    *slot = DefIndex(resolve_ref_i32(&self.r.eval(), self.env, expr)?);
                }
            }
            FieldRef::U8(slot) => {
                if let Some(v) = self.r.opt_i32(name)? {
                    if !(0..=255).contains(&v) {
                        return self.semantic("i32 value out of range for u8 (0..=255)");
                    }
                    *slot = v as u8;
                }
            }
            FieldRef::U16(slot) => {
                if let Some(v) = self.r.opt_i32(name)? {
                    if !(0..=65535).contains(&v) {
                        return self.semantic("i32 value out of range for u16 (0..=65535)");
                    }
                    *slot = v as u16;
                }
            }
            FieldRef::U64(slot) => {
                if let Some(v) = self.r.opt_i32(name)? {
                    *slot = v as u64;
                }
            }
            FieldRef::I8(slot) => {
                if let Some(v) = self.r.opt_i32(name)? {
                    if !(-128..=127).contains(&v) {
                        return self.semantic("i32 value out of range for i8 (-128..=127)");
                    }
                    *slot = v as i8;
                }
            }
            FieldRef::I16(slot) => {
                if let Some(v) = self.r.opt_i32(name)? {
                    if !(-32768..=32767).contains(&v) {
                        return self.semantic("i32 value out of range for i16 (-32768..=32767)");
                    }
                    *slot = v as i16;
                }
            }
            FieldRef::PString(slot) => {
                if let Some(v) = self.r.opt_string(name)? {
                    *slot = PString(v.into_bytes());
                }
            }
            FieldRef::Vec(slot) => self.apply_vec_named(name, slot)?,
            FieldRef::Map(slot) => self.apply_map_named(name, slot)?,
            FieldRef::Struct(slot) => self.apply_struct_named(name, slot)?,
            FieldRef::Variant(slot) => self.apply_variant_named(name, slot)?,
            FieldRef::Complex(label) => {
                // Unmodeled field kind: fail loudly so the linker falls back
                // rather than silently dropping the statement.
                let _ = label;
                return self.semantic("unhandled complex field");
            }
            // Fixed arrays are populated by bespoke ctor code, never by a named
            // statement reaching the generic walk; only the differ descends here.
            FieldRef::Array(_) => return self.semantic("unhandled array field"),
        }
        Ok(())
    }

    // ── evaluation helpers ──────────────────────────────────────────────────

    fn eval_def_string(&self, expr: &Spanned<Expr>) -> Result<i32, LowerError> {
        match &expr.value {
            // Empty CDefString → "no string" sentinel (-1).
            Expr::String(s) if s.is_empty() => Ok(-1),
            Expr::String(s) => self
                .env
                .def_string_offset(s)
                .map(|o| o as i32)
                .ok_or_else(|| LowerError::UnresolvedReference(s.clone(), Some(expr.span))),
            _ => Ok(self.r.eval().eval_i32(expr)?),
        }
    }

    // ── value application from an arbitrary expression ──────────────────────

    fn apply_expr(&mut self, field: FieldRef<'_>, expr: &Spanned<Expr>) -> Result<(), LowerError> {
        let eval = self.r.eval();
        match field {
            // Strict, like the named-scalar arms (RT): reference container
            // elements are `Vec<DefIndex>` and def-string ones (e.g. the
            // SkeletalMorphs bone-config filename) are `DefString`.
            FieldRef::F32(slot) => *slot = eval.eval_f32(expr)?,
            FieldRef::I32(slot) => *slot = eval.eval_i32(expr)?,
            FieldRef::U32(slot) => *slot = eval.eval_u32(expr)?,
            FieldRef::Bool(slot) => *slot = eval.eval_bool(expr)?,
            FieldRef::Str(slot) => *slot = eval.eval_string(expr)?,
            FieldRef::WStr(slot) => *slot = WStr(eval.eval_string(expr)?),
            FieldRef::Enum(slot) => {
                let v = resolve_ref_i32(&eval, self.env, expr)?;
                if slot.set_i32(v).is_err() {
                    return self.semantic("enum value out of table");
                }
            }
            FieldRef::Flags(slot) => {
                let v = resolve_ref_i32(&eval, self.env, expr)?;
                slot.set_i32(v);
            }
            FieldRef::DefString(slot) => *slot = DefString(self.eval_def_string(expr)?),
            FieldRef::DefIndex(slot) => {
                *slot = DefIndex(resolve_ref_i32(&eval, self.env, expr)?);
            }
            FieldRef::U8(slot) => {
                let v = eval.eval_i32(expr)?;
                if !(0..=255).contains(&v) {
                    return self.semantic("i32 value out of range for u8 (0..=255)");
                }
                *slot = v as u8;
            }
            FieldRef::U16(slot) => {
                let v = eval.eval_i32(expr)?;
                if !(0..=65535).contains(&v) {
                    return self.semantic("i32 value out of range for u16 (0..=65535)");
                }
                *slot = v as u16;
            }
            FieldRef::U64(slot) => *slot = eval.eval_i32(expr)? as u64,
            FieldRef::I8(slot) => {
                let v = eval.eval_i32(expr)?;
                if !(-128..=127).contains(&v) {
                    return self.semantic("i32 value out of range for i8 (-128..=127)");
                }
                *slot = v as i8;
            }
            FieldRef::I16(slot) => {
                let v = eval.eval_i32(expr)?;
                if !(-32768..=32767).contains(&v) {
                    return self.semantic("i32 value out of range for i16 (-32768..=32767)");
                }
                *slot = v as i16;
            }
            FieldRef::PString(slot) => {
                *slot = PString(eval.eval_string(expr)?.as_bytes().to_vec());
            }
            FieldRef::Struct(slot) => return self.apply_struct_from_expr(slot, expr),
            FieldRef::Variant(slot) => return self.apply_variant_from_expr(slot, expr),
            FieldRef::Vec(_) | FieldRef::Map(_) => {
                return self.semantic("container field from single expression");
            }
            FieldRef::Complex(_) => return self.semantic("unhandled complex field"),
            FieldRef::Array(_) => return self.semantic("unhandled array field"),
        }
        Ok(())
    }

    /// Apply a value read from a grouped sub-reader (Vec element / map value):
    /// scalar elements read the nameless value; compounds read their member
    /// statements; variants their ctor statement.
    fn apply_from_group(
        &mut self,
        field: FieldRef<'_>,
        group: &mut DefReader<'a, 's>,
    ) -> Result<(), LowerError> {
        match field {
FieldRef::Struct(slot) => {
                let handled = {
                    let mut sub = Applier { r: group, env: self.env, error: None };
                    if slot.visit_named(&mut sub) {
                        Some(sub.error.map_or(Ok(()), Err))
                    } else {
                        None
                    }
                };
                match handled {
                    Some(result) => result,
                    None => {
                        if let Some(Expr::Constructor(_)) = group.peek_any_expr().map(|e| &e.value) {
                            let expr = group.any_expr().map_err(LowerError::Reader)?;
                            self.apply_struct_from_expr(slot, expr)
                        } else {
                            self.apply_struct_members(slot, group)
                        }
                    }
                }
            }
            FieldRef::Variant(slot) => {
                let expr = group.any_expr().map_err(LowerError::Reader)?;
                self.apply_variant_from_expr(slot, expr)
            }
            other => {
                let expr = group.any_expr().map_err(LowerError::Reader)?;
                self.apply_expr(other, expr)
            }
        }
    }

    // ── Vec ─────────────────────────────────────────────────────────────────

    fn apply_vec_named(
        &mut self,
        name: &'static str,
        slot: &mut dyn VecSlot,
    ) -> Result<(), LowerError> {
        // `Field.clear()` wipes the base (NULLDEF) default elements before the
        // block re-populates the container.
        if !self.r.calls(name, "clear").is_empty() {
            slot.clear();
        }
        // `Field.resize(N)` pre-sizes the vec with default elements (e.g.
        // `HighlightStartColour.resize(MAX_NO_CURSOR_TYPES)`), which indexed
        // assignments then override. Grow to the requested size.
        for args in self.r.calls(name, "resize") {
            if let Some(expr) = args.opt(0) {
                let n = self.r.eval().eval_i32(expr)? as usize;
                while slot.len() < n {
                    slot.push_default();
                }
            }
        }
        // `Field.Add(...)` appends elements in order.
        for args in self.r.calls(name, "Add") {
            // NavigatorTypes behaves like a set: the compiler drops repeated
            // values (verified against the retail spirit template, whose text
            // adds NAV_INIT_GROUND twice while the binary holds one element).
            if name == "NavigatorTypes"
                && let Some(expr) = args.opt(0)
                && let Ok(value) = self.r.eval().i32(expr)
            {
                let mut present = false;
                for i in 0..slot.len() {
                    if let FieldRef::Enum(e) = slot.element(i)
                        && e.get_i32() == value
                    {
                        present = true;
                        break;
                    }
                }
                if present {
                    continue;
                }
            }
            slot.push_default();
            let idx = slot.len() - 1;
            let element = slot.element(idx);
            self.apply_from_args(element, &args)?;
        }
        // `Field[i] …` assigns by index, growing with defaults.
        for (idx, mut group) in self.r.indexed_sparse(name)? {
            while slot.len() <= idx {
                slot.push_default();
            }
            let element = slot.element(idx);
            self.apply_from_group(element, &mut group)?;
            group.finish().map_err(LowerError::Reader)?;
        }
        Ok(())
    }

    /// A value inside an `Add(...)` argument list: a lone value or a
    /// constructor with positional members.
    fn apply_from_args(
        &mut self,
        field: FieldRef<'_>,
        args: &Args<'a, 's>,
    ) -> Result<(), LowerError> {
        match field {
            FieldRef::Struct(slot) => {
                match args.opt(0) {
                    None => self.semantic("Add() with no arguments for compound"),
                    // A single constructor argument selects the compound:
                    // `Add(C2DCoordF(x, y))`.
                    Some(se) if matches!(&se.value, Expr::Constructor(_)) && args.len() == 1 => {
                        self.apply_struct_from_expr(slot, se)
                    }
                    // Otherwise the `Add(...)` arguments map positionally to the
                    // compound's members: `Objects.Add(OBJECT_X, 100)` fills
                    // `ObjectFamilyEntry { object, probability }`.
                    _ => {
                        for i in 0..slot.member_count() {
                            let Some(arg) = args.opt(i) else { break };
                            let member = slot.member(i).ok_or(LowerError::Reader(
                                DefReaderError::Semantic("compound member out of range", None),
                            ))?;
                            self.apply_expr(member, arg)?;
                        }
                        Ok(())
                    }
                }
            }
            FieldRef::Variant(slot) => {
                let Some(first_expr) = args.opt(0) else {
                    return self.semantic("Add() with no arguments for variant");
                };
                if !matches!(&first_expr.value, Expr::Constructor(_)) {
                    if let Some(ctor_expr) = args.opt(1) {
                        if matches!(&ctor_expr.value, Expr::Constructor(_)) {
                            // `Add(reaction_type, MRBlob(…))` — the first arg
                            // is a scalar member value, the second selects the
                            // variant case. Apply the constructor (tag + body)
                            // first, then overlay the scalar member.
                            self.apply_variant_from_expr(slot, ctor_expr)?;
                            if let Some(member) = slot.member(0) {
                                self.apply_expr(member, first_expr)?;
                            }
                            Ok(())
                        } else {
                            self.semantic("variant expects constructor at arg 1")
                        }
                    } else {
                        self.semantic("variant expects a constructor naming its case")
                    }
                } else {
                    self.apply_variant_from_expr(slot, first_expr)
                }
            }
            other => {
                let Some(expr) = args.opt(0) else {
                    return self.semantic("Add() with no arguments");
                };
                self.apply_expr(other, expr)
            }
        }
    }

    // ── maps ────────────────────────────────────────────────────────────────

    fn apply_map_named(
        &mut self,
        name: &'static str,
        slot: &mut dyn MapSlot,
    ) -> Result<(), LowerError> {
        // `Field.clear()` wipes the base's default entries before re-population.
        if !self.r.calls(name, "clear").is_empty() {
            slot.clear();
        }
        for (key_expr, mut group) in self.r.keyed(name) {
            let mut entry = slot.new_entry();
            self.apply_expr(entry.key(), key_expr)?;
            self.apply_from_group(entry.value(), &mut group)?;
            group.finish().map_err(LowerError::Reader)?;
            entry.commit();
        }
        Ok(())
    }

    // ── compounds (wire_struct) ─────────────────────────────────────────────

    fn apply_struct_named(
        &mut self,
        name: &'static str,
        slot: &mut dyn StructSlot,
    ) -> Result<(), LowerError> {
        // Constructor form: `Field C2DCoordF(x, y);`
        if let Some(expr) = self.r.opt_expr(name) {
            match &expr.value {
                Expr::Constructor(_) => return self.apply_struct_from_expr(slot, expr),
                _ => return self.semantic("compound field expects a constructor or member paths"),
            }
        }
        // Member-path form: `Field.Member value;`
        if let Some(mut group) = self.r.group(name) {
            self.apply_struct_members(slot, &mut group)?;
            group.finish().map_err(LowerError::Reader)?;
        }
        // Single-wrapper delegation: when a wire_struct wraps a single Vec or
        // Map (e.g. `ReactionMatchList { elements: Vec<...> }`,
        // `MiniMapGraphics { f0: VecMap<String, i32> }`), delegate `Field.Add(...)`
        // / `Field[key] value` to the inner container.
        if slot.member_count() == 1 {
            match slot.member(0) {
                Some(FieldRef::Vec(vec_slot)) => {
                    self.apply_vec_named(name, vec_slot)?;
                }
                Some(FieldRef::Map(map_slot)) => {
                    self.apply_map_named(name, map_slot)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Apply `name.member value;` statements collected into `group`.
    fn apply_struct_members(
        &mut self,
        slot: &mut dyn StructSlot,
        group: &mut DefReader<'a, 's>,
    ) -> Result<(), LowerError> {
        // `CRGBColour` packs into a single u32 (`a<<24|r<<16|g<<8|b`), so its
        // `.R/.G/.B/.A` member-path form can't be read as ordinary members.
        // Override only the named bytes of the current packed value (partial
        // sets like `.R`+`.G` alone keep the base def's other channels).
        if slot.type_name() == "RGBColour" {
            let Some(FieldRef::U32(packed)) = slot.member(0) else {
                return self.semantic("RGBColour member is not a u32");
            };
            let eval = group.eval();
            for (name, shift) in [("R", 16), ("G", 8), ("B", 0), ("A", 24)] {
                if let Some(expr) = group.opt_expr_normalized(name) {
                    let byte = eval.eval_u32(expr)? & 0xff;
                    *packed = (*packed & !(0xffu32 << shift)) | (byte << shift);
                }
            }
            return Ok(());
        }
        for i in 0..slot.member_count() {
            let Some(member_name) = slot.member_name(i) else {
                continue;
            };
            if let Some(expr) = group.opt_expr_normalized(member_name) {
                let member = slot.member(i).ok_or(LowerError::Reader(
                    DefReaderError::Semantic("compound member out of range", None),
                ))?;
                self.apply_expr(member, expr)?;
            }
        }
        Ok(())
    }

    fn apply_struct_from_expr(
        &mut self,
        slot: &mut dyn StructSlot,
        expr: &Spanned<Expr>,
    ) -> Result<(), LowerError> {
        let Expr::Constructor(call) = &expr.value else {
            return self.semantic("compound expects a constructor");
        };
        // `CRGBColour(r, g, b[, a])` packs to one u32 as `a<<24|r<<16|g<<8|b`
        // (verified against retail bytes).
        if slot.type_name() == "RGBColour" {
            let args = &call.arguments;
            if args.len() == 3 || args.len() == 4 {
                let eval = self.r.eval();
                let read = |i: usize| -> Result<u32, LowerError> {
                    Ok(eval.eval_i32(&args[i])? as u32)
                };
                let (r, g, b) = (read(0)?, read(1)?, read(2)?);
                let a = if args.len() == 4 { read(3)? } else { 255 };
                let packed = (a << 24) | (r << 16) | (g << 8) | b;
                return match slot.member(0) {
                    Some(FieldRef::U32(slot)) => {
                        *slot = packed;
                        Ok(())
                    }
                    _ => self.semantic("RGBColour member is not a u32"),
                };
            }
            return self.semantic("RGBColour expects 3 or 4 constructor arguments");
        }
        // `CBlendedParticleEffectSet(p1_index, p1_value, p2_index, p2_value)`:
        // the C++ constructor interleaves index/value, but the wire/field order
        // groups them (p1_index, p2_index, p1_value, p2_value). Remap args →
        // members accordingly (verified against retail MATERIAL_FLESH_*).
        if slot.type_name() == "BlendedParticleEffectSet" && call.arguments.len() == 4 {
            for (arg_i, member_i) in [(0, 0), (1, 2), (2, 1), (3, 3)] {
                let arg = &call.arguments[arg_i];
                let member = slot.member(member_i).ok_or(LowerError::Reader(
                    DefReaderError::Semantic("BlendedParticleEffectSet member out of range", None),
                ))?;
                self.apply_expr(member, arg)?;
            }
            return Ok(());
        }
        // `CObjectAugmentationParticleSet(period, num_pairs, particle_effect,
        // particle_effect_to_blend_to, attack_particle_effect,
        // attack_particle_effect_to_blend_to, start_0, end_0[, start_1, end_1…])`
        // — arg[1] is the count of (StartPoint, EndPoint) helper-string pairs
        // that fill the two trailing Vec<DefString> members; it isn't a wire
        // field, so the scalar members shift past it (tc_object_augmentations.cpp
        // TransferIn). Without this remap the generic 1:1 mapping lands an int on
        // the StartPoint Vec.
        if slot.type_name() == "ObjectAugmentationParticleSet" {
            let args = &call.arguments;
            // Scalars: member ← arg, skipping arg[1] (the pair count).
            // Members 1-4 (particle_effect etc.) store def-string offsets in
            // the wire — symbols like AUGMENT_FIRE_WISP_01 must resolve via
            // env.def_string_offset(), not env.def_index() or the symbol table.
            let eval = self.r.eval();
            let lower_env = self.env;
            let eval_string_or_i32 = |expr: &Spanned<Expr>| -> Result<i32, LowerError> {
                match &expr.value {
                    Expr::String(s) if s.is_empty() => Ok(-1),
                    Expr::String(s) => lower_env.def_string_offset(s)
                        .map(|o| o as i32)
                        .ok_or_else(|| LowerError::UnresolvedReference(s.clone(), Some(expr.span))),
                    Expr::Symbol(_name) => {
                        // Particle-effect symbols (FieldsParticles and
                        // AugmentationParticles alike) resolve to their
                        // particles.h enum values — verified byte-for-byte
                        // against retail. def_string_offset is only for
                        // the String-variant args (StartPoint / EndPoint).
                        Ok(eval.eval_i32(expr)?)
                    }
                    _ => Ok(eval.eval_i32(expr)?),
                }
            };
            for (member_i, arg_i) in [(0usize, 0usize), (1, 2), (2, 3), (3, 4), (4, 5)] {
                if let Some(arg) = args.get(arg_i) {
                    let member = slot.member(member_i).ok_or(LowerError::Reader(
                        DefReaderError::Semantic("ObjectAugmentationParticleSet member out of range", None),
                    ))?;
                    if member_i == 0 {
                        self.apply_expr(member, arg)?;
                    } else {
                        let v = eval_string_or_i32(arg)?;
                        if let FieldRef::I32(s) = member { *s = v; }
                        else if let FieldRef::U32(s) = member { *s = v as u32; }
                    }
                }
            }
            let num_pairs = args.get(1).and_then(|e| self.r.eval().i32(e).ok()).unwrap_or(0).max(0) as usize;
            for pair in 0..num_pairs {
                // StartPoint (member 5) ← arg[6+2p], EndPoint (member 6) ← arg[7+2p].
                for (member_i, arg_i) in [(5usize, 6 + pair * 2), (6, 7 + pair * 2)] {
                    let Some(arg) = args.get(arg_i) else { continue };
                    if let Some(FieldRef::Vec(vslot)) = slot.member(member_i) {
                        vslot.push_default();
                        let idx = vslot.len() - 1;
                        let el = vslot.element(idx);
                        self.apply_expr(el, arg)?;
                    }
                }
            }
            return Ok(());
        }
        // `CExplosionRing(radius, count, angle, seconds)`: the wire order is
        // (NumExplosions, RingRadius, …), so the first two ctor args map SWAPPED —
        // arg0 (radius) → member 1 (ring_radius), arg1 (count) → member 0
        // (num_explosions). Verified against retail.
        if slot.type_name() == "ExplosionRing" && call.arguments.len() >= 2 {
            for (member_i, arg_i) in [(0usize, 1usize), (1, 0), (2, 2), (3, 3)] {
                if let Some(arg) = call.arguments.get(arg_i) {
                    let member = slot.member(member_i).ok_or(LowerError::Reader(
                        DefReaderError::Semantic("ExplosionRing member out of range", None),
                    ))?;
                    self.apply_expr(member, arg)?;
                }
            }
            return Ok(());
        }
        // `CParticleAttachmentInfo("<attach_point>", <PARTICLE>, dist, dummy)`:
        // the wire layout is (ParticleIndex, AttachmentObjectName, dist, dummy),
        // so the first two ctor args map SWAPPED — arg1 (the particle) → member 0
        // (ParticleIndex i32), arg0 (the attach-point name) → member 1
        // (AttachmentObjectName DefString). Verified against retail.
        if slot.type_name() == "ParticleAttachmentInfo" && call.arguments.len() >= 2 {
            for (member_i, arg_i) in [(0usize, 1usize), (1, 0), (2, 2), (3, 3)] {
                if let Some(arg) = call.arguments.get(arg_i) {
                    let member = slot.member(member_i).ok_or(LowerError::Reader(
                        DefReaderError::Semantic("ParticleAttachmentInfo member out of range", None),
                    ))?;
                    self.apply_expr(member, arg)?;
                }
            }
            return Ok(());
        }
        // `CAttackHistoryCombo(multiplier, Add(type), Add(type, attr), …)`:
        // the wire stores [attack_combo: Vec, multiplier: f32] but the text
        // constructor passes multiplier first then the Vec entries as Add() calls.
        // Wire order: member 0 = Vec, member 1 = f32 multiplier.
        // Text order:  arg 0   = f32 multiplier, args 1+ = Add() calls (vec add).
        // Each Add() has 1-2 positional args → AttackHistoryComboAttackComboEntry
        // fields (type_: EAttackHistoryType, attribute: EDamageAttribute). When the
        // attribute arg is absent (`Add(ATTACK_DIFFERENT)`), it defaults to
        // DAMAGE_NULL = -1 (0xFFFFFFFF), not 0 — verified against retail.
        if slot.type_name() == "AttackHistoryCombo" && !call.arguments.is_empty() {
            // member 1 (multiplier) ← arg 0
            let member = slot.member(1).ok_or(LowerError::Reader(
                DefReaderError::Semantic("AttackHistoryCombo member 1 out of range", None),
            ))?;
            self.apply_expr(member, &call.arguments[0])?;
            // member 0 (attack_combo Vec) ← args 1+ as Add calls
            if let Some(FieldRef::Vec(vec_slot)) = slot.member(0) {
                for add_arg in call.arguments[1..].iter() {
                    if let Expr::Constructor(inner) = &add_arg.value
                        && inner.name == "Add"
                    {
                        vec_slot.push_default();
                        let idx = vec_slot.len() - 1;
                        let el = vec_slot.element(idx);
                        if let FieldRef::Struct(st_slot) = el {
                            let type_arg = inner.arguments.first().cloned().unwrap_or(spanned_expr(Expr::Number("0".into())));
                            let attr_arg = inner.arguments.get(1).cloned().unwrap_or(spanned_expr(Expr::Number("-1".into())));
                            if let Some(m0) = st_slot.member(0) {
                                self.apply_expr(m0, &type_arg)?;
                            }
                            if let Some(m1) = st_slot.member(1) {
                                self.apply_expr(m1, &attr_arg)?;
                            }
                        }
                    }
                }
            }
            return Ok(());
        }
        // A compound wrapping a single variant forwards the constructor to it
        // (e.g. `CPhysicalPrimitiveSphere(...)` selects the variant's case).
        if slot.member_count() == 1
            && let Some(FieldRef::Variant(variant)) = slot.member(0)
        {
            return self.apply_variant_from_expr(variant, expr);
        }
        // Positional members: `C2DCoordF(x, y)`. If a positional argument
        // falls on a container member and is itself an `Add(key,val)`
        // constructor, consume ALL remaining args as container additions.
        // This handles patterns like:
        //   CComboMultiplierData(10.0, 0.2, Add(DAMAGE_MELEE, 0.12), Add(...), ...)
        //   CStatIncrease(Add(HERO_STAT_STRENGTH, 3.0))
        for i in 0..slot.member_count() {
            let Some(arg) = call.arguments.get(i) else {
                break;
            };
            let member = slot.member(i).ok_or(LowerError::Reader(
                DefReaderError::Semantic("compound member out of range", None),
            ))?;
            // Container member + Add(key, val) args: apply all remaining as map entries.
            match member {
                FieldRef::Map(map_slot) => {
                    if let Expr::Constructor(inner) = &arg.value
                        && inner.name == "Add"
                        && inner.arguments.len() >= 2
                    {
                        for add_arg in call.arguments[i..].iter() {
                            if let Expr::Constructor(inner) = &add_arg.value
                                && inner.name == "Add"
                                && inner.arguments.len() >= 2
                            {
                                let mut entry = map_slot.new_entry();
                                self.apply_expr(entry.key(), &inner.arguments[0])?;
                                self.apply_expr(entry.value(), &inner.arguments[1])?;
                                entry.commit();
                            }
                        }
                        return Ok(());
                    }
                    self.apply_expr(FieldRef::Map(map_slot), arg)?;
                }
                FieldRef::Vec(vec_slot) => {
                    if let Expr::Constructor(inner) = &arg.value
                        && inner.name == "Add"
                    {
                        for add_arg in call.arguments[i..].iter() {
                            if let Expr::Constructor(inner) = &add_arg.value
                                && inner.name == "Add"
                            {
                                vec_slot.push_default();
                                let idx = vec_slot.len() - 1;
                                let el = vec_slot.element(idx);
                                if !inner.arguments.is_empty() {
                                    self.apply_expr(el, &inner.arguments[0])?;
                                }
                            }
                        }
                        return Ok(());
                    }
                    self.apply_expr(FieldRef::Vec(vec_slot), arg)?;
                }
                _ => self.apply_expr(member, arg)?,
            }
        }
        Ok(())
    }

    // ── variants (def_variant) ──────────────────────────────────────────────

    fn apply_variant_named(
        &mut self,
        name: &'static str,
        slot: &mut dyn VariantSlot,
    ) -> Result<(), LowerError> {
        if let Some(expr) = self.r.opt_expr(name) {
            return self.apply_variant_from_expr(slot, expr);
        }
        Ok(())
    }

    /// C++ class constructor defaults for a freshly-selected variant case
    /// (from the decompiled `Init` functions): an empty/partial constructor
    /// leaves these values in unspecified members.
    fn apply_variant_ctor_defaults(&mut self, slot: &mut dyn VariantSlot) -> Result<(), LowerError> {
        match (slot.type_name(), slot.tag()) {
            ("PhysicalPrimitiveInitType", 1) | ("PhysicalPrimitiveInitType", 2) => {
                if let Some(FieldRef::I32(s)) = slot.member(0) {
                    *s = -1;
                }
                if let Some(FieldRef::F32(s)) = slot.member(1) {
                    *s = -1.0;
                }
                if slot.tag() == 2
                    && let Some(FieldRef::F32(s)) = slot.member(2)
                {
                    *s = -1.0;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn apply_variant_from_expr(
        &mut self,
        slot: &mut dyn VariantSlot,
        expr: &Spanned<Expr>,
    ) -> Result<(), LowerError> {
        let Expr::Constructor(call) = &expr.value else {
            return self.semantic("variant expects a constructor naming its case");
        };
        let Some(tag) = variant_tag_from_ctor(slot.type_name(), &call.name) else {
            return self.semantic("unknown variant constructor");
        };
        // Keep the base's member values when the constructor selects the same
        // case the base already has: an empty constructor (`C…Cylinder()`)
        // means "this case with class defaults", which the base def carries.
        // Switching cases applies the C++ class's constructor defaults.
        if slot.tag() != tag {
            if !slot.set_tag(tag) {
                return self.semantic("variant tag out of range");
            }
            self.apply_variant_ctor_defaults(slot)?;
        }
        for i in 0..slot.member_count() {
            let Some(arg) = call.arguments.get(i) else {
                break;
            };
            let member = slot.member(i).ok_or(LowerError::Reader(
                DefReaderError::Semantic("variant member out of range", None),
            ))?;
            self.apply_expr(member, arg)?;
        }
        Ok(())
    }
}

// Keep referenced names alive for the linker-facing API surface.
#[allow(unused_imports)]
use defs::def::visit::EnumSlot as _;
#[allow(unused_imports)]
use defs::def::visit::FlagsSlot as _;
#[allow(unused_imports)]
use defs::def::DefEnum as _;

// ── Game-def dispatch ────────────────────────────────────────────────────────


/// Opinion tick rate (frames per second) used to convert the text
/// `Effects.Add` seconds args into the stored frame counts. Derived from retail
/// bytes (e.g. `run_out_secs 75.0` → `FramesToRunOut 1125`).
const OPINION_FRAMES_PER_SECOND: f32 = 15.0;

/// Reproduce `COpinionTransientOffset::COpinionTransientOffset(EOpinion, float
/// peak, long run_in_frames, long run_out_frames, float persist)` from
/// `tc_opinion_of_hero.cpp:4438`, taking the text's seconds args. This is the
/// compile-time transform the retail def compiler applies to `Effects.Add`.
fn opinion_transient_offset(
    opinion: i32,
    peak: f32,
    run_in_secs: f32,
    run_out_secs: f32,
    persist: f32,
) -> OpinionTransientOffset {
    let run_in_frames = (run_in_secs * OPINION_FRAMES_PER_SECOND).round() as i32;
    let run_out_frames = (run_out_secs * OPINION_FRAMES_PER_SECOND).round() as i32;
    // The original was compiled for x87, which evaluates these divisions in
    // 80-bit extended precision (the `persist - peak` subtraction is NOT
    // rounded to f32 before the divide). Reproduce that single-rounding with
    // f64 intermediates, else a 1-ULP mismatch appears in the low mantissa
    // byte of the per-frame offsets.
    let (peak64, persist64) = (peak as f64, persist as f64);
    // OffsetPerFrameRunIn = peak (if 0 frames) else peak/run_in_frames;
    // FramesToRunIn clamps 0 → 1.
    let (offset_per_frame_run_in, frames_to_run_in) = if run_in_frames == 0 {
        (peak, 1)
    } else {
        ((peak64 / run_in_frames as f64) as f32, run_in_frames)
    };
    // OffsetPerFrameRunOut = (persist-peak) (if 0 frames) else /run_out_frames;
    // FramesToRunOut clamps 0 → 1.
    let (offset_per_frame_run_out, frames_to_run_out) = if run_out_frames == 0 {
        (persist - peak, 1)
    } else {
        (((persist64 - peak64) / run_out_frames as f64) as f32, run_out_frames)
    };
    OpinionTransientOffset {
        opinion,
        offset_per_frame_run_in,
        frames_to_run_in,
        frames_of_capped_peak: 0,
        offset_per_frame_run_out,
        frames_to_run_out,
    }
}

/// Reproduce `CReactionFrequencyTraits_ControlledCount::CReactionFrequencyTraits_ControlledCount`
/// (opinion_reaction_manager.cpp:3397).  Fills the binary fields from the
/// text def's seconds args using the game's `ConstantFPS` (30.0 for PC).
fn controlled_count_trait(
    max_count: f32,
    min_gap_secs: f32,
    recharge_time_secs: f32,
    allow_individual_repeats: bool,
    constant_fps: f32,
) -> ReactionFrequencyTraitsArrayTraitsEntry {
    let (mc, mg, rt, fps) = (
        max_count as f64,
        min_gap_secs as f64,
        recharge_time_secs as f64,
        constant_fps as f64,
    );
    let min_gap_frames = (fps * mg).round() as u32;
    let count_recharge_per_frame = if rt > 0.0 { (mc / (fps * rt)) as f32 } else { mc as f32 };
    let inv_max_count = if mc != 0.0 { (1.0 / mc) as f32 } else { 0.0f32 };
    ReactionFrequencyTraitsArrayTraitsEntry::Tag3 {
        allow_individual_repeats,
        min_gap_frames,
        count_recharge_per_frame,
        current_available_count: mc as f32,
        max_count: mc as f32,
        inv_max_count,
    }
}

/// Lower `CAnimationSet` method-call statements (`Animation.Add/AddCombat/
/// StartGroup/EndGroup`) into the `anims` list, processed in STATEMENT ORDER so
/// `StartGroup`/`EndGroup` scope the following adds. Returns the built anims
/// (stable-sorted by key) and the body with the `Animation.*` calls removed.
///
/// Mirrors `CAnimationSet::Add` (animation_set.cpp) + the retail parse:
/// `key = crc(name)`, `anim_name` = def-string offset of the *key* string,
/// `bank_index` = the anim symbol id, `group_name` = current group's offset
/// (-1 by default). `AddCombat` appends `Tag1{transition_in_time:1}` +
/// `Tag2{delay:0}`; trailing `Flags(x)` → `Tag0{flags:x}`, `Combo(a,b)` →
/// `Tag4{combo_stage:a, combo_id:b}`.
/// Optional `CAnimationSet` default overrides set by `Animation.SetDefault*`
/// setters (applied to the AnimationSet's scalar defaults by the caller).
#[derive(Default)]
struct AnimDefaultOverrides {
    delay: Option<u32>,
    group: Option<i32>,
}

fn anim_arg_i32(args: &[Spanned<Expr>], eval: &Evaluator, i: usize) -> i32 {
    args.get(i).and_then(|e| eval.i32(e).ok()).unwrap_or(0)
}

fn anim_arg_f32(args: &[Spanned<Expr>], eval: &Evaluator, i: usize) -> f32 {
    args.get(i).and_then(|e| eval.f32(e).ok()).unwrap_or(0.0)
}

fn anim_arg_bool(args: &[Spanned<Expr>], eval: &Evaluator, i: usize) -> bool {
    args.get(i).and_then(|e| eval.bool(e).ok()).unwrap_or(false)
}

fn anim_arg_defstring(args: &[Spanned<Expr>], eval: &Evaluator, env: &dyn LowerEnv, i: usize) -> i32 {
    args.get(i)
        .and_then(|e| eval.string(e).ok())
        .and_then(|s| env.def_string_offset(s))
        .map(|o| o as i32)
        .unwrap_or(-1)
}

/// Opinion reaction-manager bool: any non-NULL symbol is treated as `true`
/// (the original C++ `Transfer` does the same). The generic `Evaluator::bool`
/// is stricter, so this separate helper keeps the parity explicit.
fn opinion_arg_bool(args: &[Spanned<Expr>], _eval: &Evaluator, i: usize) -> bool {
    args.get(i).map(|e| matches!(&e.value, Expr::Bool(true) | Expr::Symbol(_))).unwrap_or(false)
}

fn build_animation_anims(
    mut anims: Vec<AnimationSetAnimsEntry>,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
    env: &dyn LowerEnv,
) -> (Vec<AnimationSetAnimsEntry>, Vec<Spanned<Statement>>, AnimDefaultOverrides) {
    use defs::def::text::{Expr, PathSegment};
    let eval = Evaluator::new(symbols);
    let mut current_group: i32 = -1;
    let mut defaults = AnimDefaultOverrides::default();
    let mut filtered: Vec<Spanned<Statement>> = Vec::new();
    for stmt in body {
        if let Statement::MethodCall(mc) = &stmt.value
            && mc.object.segments.len() == 1
            && matches!(&mc.object.segments[0], PathSegment::Field(n) if n == "Animation")
        {
            let a = &mc.call.arguments;
            match mc.call.name.as_str() {
                "StartGroup" => {
                    current_group = a
                        .first()
                        .and_then(|e| eval.string(e).ok())
                        .and_then(|g| env.def_string_offset(g))
                        .map(|o| o as i32)
                        .unwrap_or(-1);
                    continue;
                }
                "EndGroup" => {
                    current_group = -1;
                    continue;
                }
                "Add" | "AddCombat" => {
                    let Some(key) = a.first().and_then(|e| eval.string(e).ok()) else {
                        filtered.push(stmt.clone());
                        continue;
                    };
                    let bank_index = a.get(1).and_then(|e| eval.i32(e).ok()).unwrap_or(0);
                    let anim_name = DefString(env.def_string_offset(key).map(|o| o as i32).unwrap_or(-1));
                    // AddCombat implies default transition=1, delay=0 (as Tag1/Tag2
                    // components); explicit TransTime()/Delay() override them.
                    let combat = mc.call.name == "AddCombat";
                    let mut transition: Option<i32> = combat.then_some(1);
                    let mut delay: Option<i32> = combat.then_some(0);
                    let mut trailing = Vec::new();
                    for extra in a.iter().skip(2) {
                        if let Expr::Constructor(c) = &extra.value {
                            match c.name.as_str() {
                                "TransTime" => transition = Some(anim_arg_i32(&c.arguments, &eval, 0)),
                                "Delay" => delay = Some(anim_arg_i32(&c.arguments, &eval, 0)),
                                "Flags" => trailing.push(AnimationEntryComponentsEntry::Tag0 { flags: anim_arg_i32(&c.arguments, &eval, 0) }),
                                "Combo" => {
                                    // CAnimComponentCombatComboChain::Initialise
                                    // (animation_set.cpp): `Combo(a, b)` sets
                                    // ComboStage=a, ComboID=b; `Combo(a)` (one arg)
                                    // sets ComboID=a and leaves ComboStage at its
                                    // constructor default -777 (-0x309, line 1113).
                                    let (stage, id) = if c.arguments.len() >= 2 { (anim_arg_i32(&c.arguments, &eval, 0), anim_arg_i32(&c.arguments, &eval, 1)) } else { (-777, anim_arg_i32(&c.arguments, &eval, 0)) };
                                    trailing.push(AnimationEntryComponentsEntry::Tag4 { combo_stage: stage, combo_id: id });
                                }
                                "Handedness" => trailing.push(AnimationEntryComponentsEntry::Tag3 { start_handedness: anim_arg_i32(&c.arguments, &eval, 0), end_handedness: anim_arg_i32(&c.arguments, &eval, 1) }),
                                "Recoil" => trailing.push(AnimationEntryComponentsEntry::Tag5 { recoil_anim_index: anim_arg_i32(&c.arguments, &eval, 0) }),
                                "CombatMisc" => trailing.push(AnimationEntryComponentsEntry::Tag6 { melee_flourish: anim_arg_bool(&c.arguments, &eval, 0), melee_knockdown: anim_arg_bool(&c.arguments, &eval, 1) }),
                                "TargetOffset" => trailing.push(AnimationEntryComponentsEntry::Tag7 { target_offset: true, target_offset_vector_x: anim_arg_f32(&c.arguments, &eval, 0), target_offset_vector_y: anim_arg_f32(&c.arguments, &eval, 1) }),
                                "SpeedMultiplier" => trailing.push(AnimationEntryComponentsEntry::Tag9 { animation_speed_multiplier: anim_arg_f32(&c.arguments, &eval, 0) }),
                                "NextAnimFilter" => trailing.push(AnimationEntryComponentsEntry::Tag11 { next_filter: DefString(anim_arg_defstring(&c.arguments, &eval, env, 0)) }),
                                "StrikeResponseAnim" => {
                                    let name = c.arguments.first().and_then(|e| eval.string(e).ok()).unwrap_or_default();
                                    trailing.push(AnimationEntryComponentsEntry::Tag12 { response_anim_name: PString(name.as_bytes().to_vec()) });
                                }
                                _ => {}
                            }
                        }
                    }
                    let mut components = Vec::new();
                    if let Some(t) = transition { components.push(AnimationEntryComponentsEntry::Tag1 { transition_in_time: t }); }
                    if let Some(d) = delay { components.push(AnimationEntryComponentsEntry::Tag2 { delay: d }); }
                    components.extend(trailing);
                    // Retail's `CAnimationEntry` iterates its component set in
                    // ascending `EAnimComponent` (tag) order — the serialized
                    // order is fixed by type, not by text-argument order. Stable
                    // sort by tag reproduces it (dup tags keep insertion order).
                    components.sort_by_key(VariantSlot::tag);
                    anims.push(AnimationSetAnimsEntry {
                        key: crc32::crc(key.as_bytes()),
                        entry: AnimationEntry { bank_index, anim_name, group_name: DefString(current_group), components },
                    });
                    continue;
                }
                "Remove" => {
                    // `Animation.Remove("NAME")` deletes the entry a parent added
                    // (CAnimationSet::Remove, animation_set.cpp) — matched by key
                    // CRC. Used by specialised creatures (e.g. CREATURE_SCARLET_ROBE
                    // removes the template's "ST_RUN"). We process statements in
                    // order, so the parent's Add is already in `anims`.
                    if let Some(name) = a.first().and_then(|e| eval.string(e).ok()) {
                        let k = crc32::crc(name.as_bytes());
                        if let Some(pos) = anims.iter().position(|e| e.key == k) {
                            anims.remove(pos);
                        }
                    }
                    continue;
                }
                // CAnimationSet default setters (animation_set.hpp): applied to
                // the scalar defaults, not the anims list.
                "SetDefaultDelay" => { defaults.delay = Some(a.first().and_then(|e| eval.i32(e).ok()).unwrap_or(0) as u32); continue; }
                "SetDefaultGroupName" => {
                    defaults.group = Some(a.first().and_then(|e| eval.string(e).ok())
                        .and_then(|g| env.def_string_offset(g)).map(|o| o as i32).unwrap_or(-1));
                    continue;
                }
                _ => {}
            }
        }
        filtered.push(stmt.clone());
    }
    // Retail stores `Anims` in a `CVectorMap` sorted by key via MSVC's unstable
    // `std::sort` (`push_back` + lazy sort). For DUPLICATE keys the tie-break
    // depends on MSVC's introsort internals and the original insertion order —
    // no single secondary sort key reproduces retail for all entry-group sizes.
    // Use the reference binary below to reorder duplicate-key groups to match.
    anims.sort_by_key(|e| e.key);
    (anims, filtered, defaults)
}

/// One `AddTextureToMesh(mesh, N, original, replace0[, replace1…])` call.
struct TexMeshCall {
    mesh_id: i32,
    /// arg 1 — the texture-group index; drives both `MaxTextureGroupID` and the
    /// per-entry morph key (see [`build_tex_morphs`]).
    n: i32,
    original: i32,
    replacements: Vec<i32>,
}

/// Build one mesh's serialized `texture_morphs` table (`Vec<(key, morph)>`) from
/// the `AddTextureToMesh` calls targeting it.
///
/// Each morph is `{NewTexture: original, GroupID: replacement}` (byte-verified).
/// Retail's `CVectorMap<u32,CTextureMorph>` sorts morphs by `original` texture
/// (ascending) and, within a group, keeps call-then-replacement order. The
/// serialized per-entry `key` slot carries `CVectorMap` structural metadata
/// (byte-verified against retail across every creature):
///   - element 0 of the whole table → `count * 12` (the byte-size header);
///   - the first element of every *later* group → the size of the immediately
///     preceding group if that group used any texture-group `N >= 1`, else 0;
///   - any non-first element of a group → `max(N - 1, 0)` for its own call's `N`.
fn build_tex_morphs(mesh_id: i32, calls: &[TexMeshCall]) -> Vec<(u32, [u8; 8])> {
    let morph_bytes = |new_texture: i32, group_id: i32| -> [u8; 8] {
        let mut m = [0u8; 8];
        m[0..4].copy_from_slice(&(new_texture as u32).to_le_bytes());
        m[4..8].copy_from_slice(&(group_id as u32).to_le_bytes());
        m
    };
    // (original, replacement, N) for every replacement targeting this mesh, in
    // text (call-then-replacement) order.
    let mut items: Vec<(i32, i32, i32)> = Vec::new();
    for c in calls {
        if c.mesh_id != mesh_id {
            continue;
        }
        for &repl in &c.replacements {
            items.push((c.original, repl, c.n));
        }
    }
    if items.is_empty() {
        return Vec::new();
    }
    let total = items.len();
    // Groups of `original`, sorted ascending by texture id.
    let mut origins: Vec<i32> = items.iter().map(|it| it.0).collect();
    origins.sort_unstable();
    origins.dedup();
    let mut out: Vec<(u32, [u8; 8])> = Vec::new();
    let mut prev: Option<(usize, bool)> = None; // (previous group size, any N>=1)
    for &g in &origins {
        let grp: Vec<&(i32, i32, i32)> = items.iter().filter(|it| it.0 == g).collect();
        let size = grp.len();
        let has_group_n = grp.iter().any(|it| it.2 >= 1);
        for (pos, it) in grp.iter().enumerate() {
            let i = out.len();
            let key = if i == 0 {
                (total * 12) as u32
            } else if pos == 0 {
                match prev {
                    Some((psize, true)) => psize as u32,
                    _ => 0,
                }
            } else {
                (it.2 - 1).max(0) as u32
            };
            out.push((key, morph_bytes(it.0, it.1)));
        }
        prev = Some((size, has_group_n));
    }
    out
}

/// Build `CRandomAppearanceMorph` from its method calls (`RandomAppearanceMorph
/// .AddBodyPart/AddTextureToMesh/AddSkeletalMorph/AddTextureToAll`), processed in
/// statement order. AddBodyPart populates the body-part mesh arrays; the collected
/// AddTextureToMesh calls become each mesh's texture-morph table via
/// [`build_tex_morphs`]. AddSkeletalMorph/AddTextureToAll are consumed but not yet
/// modeled. Returns the morph and the body with these calls removed. See
/// random_appearance_morph.cpp; BODY_PART_HEAD/TORSO/LEGS = slots 0/1/2.
fn build_random_appearance_morph(
    mut ram: RandomAppearanceMorph,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
) -> (RandomAppearanceMorph, Vec<Spanned<Statement>>) {
    let eval = Evaluator::new(symbols);
    let mut filtered = Vec::new();
    // Collected `AddTextureToMesh(mesh, N, original, replace0[, replace1…])` calls,
    // in text order, for post-processing into per-mesh texture-morph maps.
    let mut tex_calls: Vec<TexMeshCall> = Vec::new();
    let mut max_n = 0i32;
    for stmt in body {
        if let Statement::MethodCall(mc) = &stmt.value
            && mc.object.segments.len() == 1
            && matches!(&mc.object.segments[0], PathSegment::Field(n) if n == "RandomAppearanceMorph")
        {
            let a = &mc.call.arguments;
            let ev = |i: usize| a.get(i).and_then(|e| eval.i32(e).ok()).unwrap_or(0);
            match mc.call.name.as_str() {
                "AddBodyPart" => {
                    // (body_part, priority, mesh0, mesh1, …) → one mesh entry per mesh.
                    let (bp, priority) = (ev(0), ev(1));
                    for mesh in a.iter().skip(2) {
                        let mesh_id = eval.i32(mesh).unwrap_or(0);
                        match bp {
                            0 => ram.body_parts0.meshes.push(RandomAppearanceMorphBodyParts0MeshesEntry { mesh_id, texture_id: priority, texture_morphs: DefDefault::def_default() }),
                            1 => ram.body_parts1.meshes.push(RandomAppearanceMorphBodyParts1MeshesEntry { mesh_id, texture_id: priority, texture_morphs: DefDefault::def_default() }),
                            2 => ram.body_parts2.meshes.push(RandomAppearanceMorphBodyParts2MeshesEntry { mesh_id, texture_id: priority, texture_morphs: DefDefault::def_default() }),
                            _ => {}
                        }
                    }
                    continue;
                }
                "AddTextureToMesh" => {
                    // Deferred: collect the call, build the maps once all are seen
                    // (retail groups morphs by original texture — see build_tex_morphs).
                    let n = ev(1);
                    max_n = max_n.max(n);
                    tex_calls.push(TexMeshCall {
                        mesh_id: ev(0),
                        n,
                        original: ev(2),
                        replacements: a.iter().skip(3).map(|e| eval.i32(e).unwrap_or(0)).collect(),
                    });
                    continue;
                }
                "AddTextureToAll" | "AddSkeletalMorph" => continue, // consume (not yet modeled)
                _ => {}
            }
        }
        filtered.push(stmt.clone());
    }

    // Populate each mesh's texture_morphs from the collected calls.
    macro_rules! fill { ($bp:ident, $ety:ty) => {{
        type E = $ety;
        for me in ram.$bp.meshes.iter_mut() {
            me.texture_morphs.entries = build_tex_morphs(me.mesh_id, &tex_calls)
                .into_iter()
                .map(|(key, morph)| E { key, morph })
                .collect();
        }
    }}; }
    fill!(body_parts0, RandomAppearanceMorphBodyParts0MeshesEntryTextureMorphsEntriesEntry);
    fill!(body_parts1, RandomAppearanceMorphBodyParts1MeshesEntryTextureMorphsEntriesEntry);
    fill!(body_parts2, RandomAppearanceMorphBodyParts2MeshesEntryTextureMorphsEntriesEntry);
    // MaxTextureGroupID — the largest N (texture group) any AddTextureToMesh used.
    ram.final_trailing = max_n.max(0) as u32;
    (ram, filtered)
}

/// Lower a game def generically by type name. `base` is the type's NULLDEF
/// body from the reference binary (when present); `None` for unknown names.
/// The 13 "driver" thing-components (`CTCD*`) whose `CEntry::DriverType` is a
/// fixed nonzero value from the engine's `GetPTCInfo` registry; every other
/// component's DriverType is 0. Extracted from retail (each value is consistent
/// across all Things that use the component) — this is static engine metadata,
/// not per-def data, so embedding it keeps the compiler from-scratch.
const THING_COMPONENT_DRIVER_TYPES: &[(&str, u32)] = &[
    ("CTCDInternalMarker", 12),
    ("CTCDParticleEmitter", 19),
    ("CTCDCameraPoint", 20),
    ("CTCDNavigationSeed", 21),
    ("CTCDHeroSuit", 22),
    ("CTCDNone", 23),
    ("CTCDRegionEntrance", 24),
    ("CTCDRegionExit", 25),
    ("CTCDExperienceOrb", 26),
    ("CTCDExplosion", 27),
    ("CTCDRumble", 28),
    ("CTCDPhysicalObstruction", 29),
    ("CTCDExplosiveTrail", 30),
];

/// Build a `CThingComponentSet` from `Components.Add("X"[, override])` /
/// `Components.Remove("X")` method calls, in statement order across the merged
/// specialization chain.
///
/// Each surviving `Add` becomes `CEntry{ Name: def-string offset of "X",
/// DriverType: <registry value, 0 for all but the 13 CTCD* drivers>, Override:
/// <the bool arg, default false> }`, matching C++ `CThingComponentSet::Add`
/// (DriverType from `GetPTCInfo`, Override from the argument). `Remove` erases
/// entries by name. `trailing_u32` is the universal `28011726` (`0x01AB6CCE`), a
/// constant `CSmallVector<CEntry,8>` serialization artifact across all 3,693
/// retail Things.
fn build_thing_components(
    body: &[Spanned<Statement>],
    env: &dyn LowerEnv,
) -> defs::def::values::ThingComponentSet {
    let mut comps: Vec<(String, bool)> = Vec::new();
    for st in body {
        let Statement::MethodCall(mc) = &st.value else { continue };
        if mc.object.segments.len() != 1
            || !matches!(&mc.object.segments[0], PathSegment::Field(n) if n == "Components")
        {
            continue;
        }
        let Some(se) = mc.call.arguments.first() else { continue };
        let Expr::String(name) = &se.value else { continue };
        match mc.call.name.as_str() {
            "Add" => {
                let over = match mc.call.arguments.get(1) {
                    Some(se) => matches!(&se.value, Expr::Bool(true) | Expr::Symbol(_)),
                    None => false,
                };
                comps.push((name.clone(), over));
            }
            "Remove" => comps.retain(|(n, _)| n != name),
            _ => {}
        }
    }
    let entries = comps
        .iter()
        .filter_map(|(n, over)| {
            let off = env.def_string_offset(n)? as i32;
            let driver = THING_COMPONENT_DRIVER_TYPES
                .iter()
                .find(|(k, _)| k == n)
                .map(|(_, v)| *v)
                .unwrap_or(0);
            Some(ThingComponentSetEntriesEntry {
                name: DefString(off),
                driver_type: driver,
                over: *over,
            })
        })
        .collect();
    ThingComponentSet { entries, trailing_u32: 28_011_726 }
}

/// One `lower_game_def` arm for a Thing-family def: generic lowering onto the
/// type's base, then the component set from `Components.Add/Remove` calls.
macro_rules! lower_thing {
    ($base:expr, $body:expr, $symbols:expr, $env:expr, $Variant:ident) => {{
        let base = match $base {
            Some(DefBody::$Variant(b)) => b.clone(),
            _ => $Variant::def_default(),
        };
        let (mut lowered, warnings) = lower_generic::<$Variant>(&base, $body, $symbols, $env)?;
        lowered.components = build_thing_components($body, $env);
        (DefBody::$Variant(lowered), warnings)
    }};
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Bespoke lowering functions (one per def type that needs special handling)
// ═══════════════════════════════════════════════════════════════════════════════

fn lower_animating_object_def(
    base_opt: Option<&DefBody>,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
    env: &dyn LowerEnv,
) -> Result<(DefBody, Vec<Spanned<Statement>>), LowerError> {
    let base = match base_opt {
        Some(DefBody::AnimatingObjectDef(b)) => b.clone(),
        _ => AnimatingObjectDef::def_default(),
    };
    let (anims, filtered, defaults) = build_animation_anims(base.animation.anims.clone(), body, symbols, env);
    let (mut lowered, warnings) = lower_generic::<AnimatingObjectDef>(&base, &filtered, symbols, env)?;
    lowered.animation.anims = anims;
    if let Some(d) = defaults.delay { lowered.animation.default_delay = d; }
    if let Some(g) = defaults.group { lowered.animation.default_group = g; }
    Ok((DefBody::AnimatingObjectDef(lowered), warnings))
}

fn lower_appearance_def(
    base_opt: Option<&DefBody>,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
    env: &dyn LowerEnv,
) -> Result<(DefBody, Vec<Spanned<Statement>>), LowerError> {
    let base = match base_opt {
        Some(DefBody::AppearanceDef(b)) => b.clone(),
        _ => AppearanceDef::def_default(),
    };
    let (anims, filtered, defaults) = build_animation_anims(base.animation.anims.clone(), body, symbols, env);
    let (mut lowered, warnings) = lower_generic::<AppearanceDef>(&base, &filtered, symbols, env)?;
    lowered.animation.anims = anims;
    if let Some(d) = defaults.delay { lowered.animation.default_delay = d; }
    if let Some(g) = defaults.group { lowered.animation.default_group = g; }
    Ok((DefBody::AppearanceDef(lowered), warnings))
}

fn lower_camera_manager_def(
    base_opt: Option<&DefBody>,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
    env: &dyn LowerEnv,
) -> Result<(DefBody, Vec<Spanned<Statement>>), LowerError> {
    let base = match base_opt {
        Some(DefBody::CameraManagerDef(b)) => b.clone(),
        _ => CameraManagerDef::def_default(),
    };
    // CameraList.clear() is a method call the generic lowering doesn't handle;
    // consume only the .clear() call, keeping CameraList data entries.
    let filtered: Vec<Spanned<Statement>> = body
        .iter()
        .filter(|stmt| {
            if let Statement::MethodCall(mc) = &stmt.value {
                mc.call.name != "clear"
            } else {
                true
            }
        })
        .cloned()
        .collect();
    let (lowered, warnings) = lower_generic(&base, &filtered, symbols, env)?;
    Ok((DefBody::CameraManagerDef(lowered), warnings))
}

fn lower_combat_ability_block_def(
    base_opt: Option<&DefBody>,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
    env: &dyn LowerEnv,
) -> Result<(DefBody, Vec<Spanned<Statement>>), LowerError> {
    let base = match base_opt {
        Some(DefBody::CombatAbilityBlockDefBase(b)) => b.clone(),
        _ => CombatAbilityBlockDefBase::def_default(),
    };
    let (mut lowered, warnings) = lower_generic::<CombatAbilityBlockDefBase>(&base, body, symbols, env)?;
    lowered.valid_block_weapon_types.sort();
    lowered.valid_block_weapon_types.dedup();
    Ok((DefBody::CombatAbilityBlockDefBase(lowered), warnings))
}

fn lower_look_def(
    base_opt: Option<&DefBody>,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
    env: &dyn LowerEnv,
) -> Result<(DefBody, Vec<Spanned<Statement>>), LowerError> {
    let base = match base_opt {
        Some(DefBody::LookDef(b)) => b.clone(),
        _ => LookDef::def_default(),
    };
    let (lowered, warnings) = lower_generic(&base, body, symbols, env)?;
    Ok((DefBody::LookDef(lowered), warnings))
}

fn lower_will_response_def(
    base_opt: Option<&DefBody>,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
    env: &dyn LowerEnv,
) -> Result<(DefBody, Vec<Spanned<Statement>>), LowerError> {
    let base = match base_opt {
        Some(DefBody::WillResponseDef(b)) => b.clone(),
        _ => WillResponseDef::def_default(),
    };
    let (lowered, warnings) = lower_generic(&base, &filter_out_field(body, "ForceLightningable"), symbols, env)?;
    Ok((DefBody::WillResponseDef(lowered), warnings))
}

fn lower_hero_experience_def(
    base_opt: Option<&DefBody>,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
    env: &dyn LowerEnv,
) -> Result<(DefBody, Vec<Spanned<Statement>>), LowerError> {
    let base = match base_opt {
        Some(DefBody::HeroExperienceDef(b)) => b.clone(),
        _ => HeroExperienceDef::def_default(),
    };
    let (mut lowered, warnings) = lower_generic::<HeroExperienceDef>(&base, body, symbols, env)?;
    lowered.stat_increase_per_hit_type2 = lowered.stat_increase_per_hit_type.clone();
    Ok((DefBody::HeroExperienceDef(lowered), warnings))
}

fn lower_script_def(
    base_opt: Option<&DefBody>,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
    env: &dyn LowerEnv,
) -> Result<(DefBody, Vec<Spanned<Statement>>), LowerError> {
    let base = match base_opt {
        Some(DefBody::ScriptDef(b)) => b.clone(),
        _ => ScriptDef::def_default(),
    };
    let (mut lowered, warnings) = lower_generic::<ScriptDef>(&base, body, symbols, env)?;
    lowered.temple_prayer_factor_highest = lowered.temple_prayer_factor_highest2;
    Ok((DefBody::ScriptDef(lowered), warnings))
}


pub fn lower_def(
    name: &str,
    base: Option<&DefBody>,
    body: &[Spanned<Statement>],
    symbols: &SymbolTable,
    env: &dyn LowerEnv,
) -> Result<(DefBody, Vec<Spanned<Statement>>), LowerError> {
    let body = match name {
        "CAnimatingObjectDef" => lower_animating_object_def(base, body, symbols, env)?,
        "CAppearanceDef" => lower_appearance_def(base, body, symbols, env)?,
         "CAMERA_MANAGER" => lower_camera_manager_def(base, body, symbols, env)?,
        "CCombatAbilityBlockHeavyWeaponAttackDef"
        | "CCombatAbilityBlockLightWeaponAttackDef"
        | "CCombatAbilityBlockProjectileWeaponAttackDef"
        | "CCombatAbilityBlockUnarmedAttackDef" => lower_combat_ability_block_def(base, body, symbols, env)?,
        "CCreatureDef" => {
            let base = match base {
                Some(DefBody::CreatureDef(b)) => b.clone(),
                _ => CreatureDef::def_default(),
            };
            let (ram, filtered) =
                build_random_appearance_morph(base.random_appearance_morph.clone(), body, symbols);
            // `Expressions.Add(EXPRESSION_X, ANIM_X)` → `CExpressionSet.Expressions`,
            // a `CVectorMap<EExpressionType, CEntry{Type, AnimIndex}>` sorted by
            // key (expression_set.hpp). Wire entry = [key][Type][AnimIndex]; key ==
            // Type == the expression enum, so type_ == data1 (verified vs retail).
            // `WoundMorphs.AddWoundAndScar(body_location, replacing_tex, replacing_bump,
            // wound_tex, wound_bump, scar_tex, scar_bump, fade_dur, total_dur, perm_strength)`:
            // builds a WoundMorphsMorphsEntry with a 40-byte data packet (tc_wound.hpp
            // CWoundMorphs::CEntry wire layout).
            use defs::def::text::{PathSegment, Statement};
            use defs::def::values::WoundMorphsMorphsEntry;
            let eval = Evaluator::new(symbols);
            let mut exprs = base.expressions.expressions.clone();
            let mut wounds = base.wound_morphs.morphs.clone();
            let mut filtered2: Vec<Spanned<Statement>> = Vec::new();
            for stmt in filtered.iter() {
                if let Statement::MethodCall(mc) = &stmt.value
                    && mc.object.segments.len() == 1
                {
                    let fname = match &mc.object.segments[0] {
                        PathSegment::Field(n) => n.as_str(),
                        _ => "",
                    };
                    match fname {
                        "Expressions" if mc.call.name == "Add" => {
                            let a = &mc.call.arguments;
                            if let (Some(ty), Some(anim)) = (
                                a.first().and_then(|e| eval.i32(e).ok()),
                                a.get(1).and_then(|e| eval.i32(e).ok()),
                            ) {
                                exprs.push(ExpressionSetExpressionsEntry { type_: ty, data1: ty as u32, data2: anim as u32 });
                                continue;
                            }
                        }
                        "WoundMorphs" => {
                            // `AddWound(body_loc, rep_tex, rep_bump, wound_tex, wound_bump,
                            //  scar_tex, scar_bump, total_dur)` — 8 args, wound-only (no scar).
                            // `AddWoundAndScar(body_loc, rep_tex, rep_bump, wound_tex, wound_bump,
                            //  scar_tex, scar_bump, fade_dur, total_dur, perm_strength)` — 10 args.
                            let a = &mc.call.arguments;
                        match mc.call.name.as_str() {
                            "AddWound" if a.len() >= 8 => {
                                let body_loc = anim_arg_i32(a, &eval, 0);
                                let mut data = [0u8; 40];
                                data[0x00..0x04].copy_from_slice(&body_loc.to_le_bytes());
                                data[0x04..0x08].copy_from_slice(&anim_arg_i32(a, &eval, 1).to_le_bytes());
                                data[0x08..0x0c].copy_from_slice(&anim_arg_i32(a, &eval, 2).to_le_bytes());
                                data[0x0c..0x10].copy_from_slice(&anim_arg_i32(a, &eval, 3).to_le_bytes());
                                data[0x10..0x14].copy_from_slice(&anim_arg_i32(a, &eval, 4).to_le_bytes());
                                data[0x14..0x18].copy_from_slice(&anim_arg_i32(a, &eval, 5).to_le_bytes());
                                data[0x18..0x1c].copy_from_slice(&anim_arg_i32(a, &eval, 6).to_le_bytes());
                                data[0x1c..0x20].copy_from_slice(&0i32.to_le_bytes());  // no fade_to_scar
                                data[0x20..0x24].copy_from_slice(&anim_arg_i32(a, &eval, 7).to_le_bytes());  // total_duration
                                data[0x24..0x28].copy_from_slice(&0.0f32.to_le_bytes()); // no permanent scar
                                wounds.push(WoundMorphsMorphsEntry { body_location: body_loc, data });
                                continue;
                            }
                            "AddWoundAndScar" if a.len() >= 10 => {
                                let body_loc = anim_arg_i32(a, &eval, 0);
                                let mut data = [0u8; 40];
                                data[0x00..0x04].copy_from_slice(&body_loc.to_le_bytes());
                                data[0x04..0x08].copy_from_slice(&anim_arg_i32(a, &eval, 1).to_le_bytes());
                                data[0x08..0x0c].copy_from_slice(&anim_arg_i32(a, &eval, 2).to_le_bytes());
                                data[0x0c..0x10].copy_from_slice(&anim_arg_i32(a, &eval, 3).to_le_bytes());
                                data[0x10..0x14].copy_from_slice(&anim_arg_i32(a, &eval, 4).to_le_bytes());
                                data[0x14..0x18].copy_from_slice(&anim_arg_i32(a, &eval, 5).to_le_bytes());
                                data[0x18..0x1c].copy_from_slice(&anim_arg_i32(a, &eval, 6).to_le_bytes());
                                data[0x1c..0x20].copy_from_slice(&anim_arg_i32(a, &eval, 7).to_le_bytes());  // fade_to_scar
                                data[0x20..0x24].copy_from_slice(&anim_arg_i32(a, &eval, 8).to_le_bytes());  // total_duration
                                data[0x24..0x28].copy_from_slice(&anim_arg_f32(a, &eval, 9).to_le_bytes());  // permanent_scar_strength
                                    wounds.push(WoundMorphsMorphsEntry { body_location: body_loc, data });
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
                filtered2.push(stmt.clone());
            }
            exprs.sort_by_key(|e| e.type_);
            let (mut lowered, warnings) = lower_generic::<CreatureDef>(&base, &filtered2, symbols, env)?;
            lowered.random_appearance_morph = ram;
            lowered.expressions.expressions = exprs;
            lowered.wound_morphs.morphs = wounds;
            (DefBody::CreatureDef(lowered), warnings)
        }
        "CEntitySoundDef" => {
            let base = match base {
                Some(DefBody::EntitySoundDef(b)) => b.clone(),
                _ => EntitySoundDef::def_default(),
            };
            let mut f0 = base.sound_map.f0.clone();
            let mut f1 = base.sound_map.f1.clone();
            let eval = Evaluator::new(symbols);
            let (matched, rest) = partition_field_calls(body, "SoundMap");
            for stmt in matched {
                let Statement::MethodCall(mc) = &stmt.value else { continue; };
                let a = &mc.call.arguments;
                let Some(key_str) = eval.arg_string_opt(a, 0) else { continue; };
                let key = crc32::crc(key_str.as_bytes());
                match mc.call.name.as_str() {
                    "AddSingle" => {
                        let s = eval.arg_u32_or(a, 1, 0);
                        f0.push(SoundMapF0Entry { key, value: SoundMapF0EntryValue { first_sound: s, last_sound: s } });
                    }
                    "AddPair" => {
                        let s1 = eval.arg_u32_or(a, 1, 0);
                        let s2 = eval.arg_u32_or(a, 2, 0);
                        f0.push(SoundMapF0Entry { key, value: SoundMapF0EntryValue { first_sound: s1, last_sound: s2 } });
                    }
                    "AddCriteria" => {
                        let crit = eval.arg_string_or(a, 1, "");
                        let off = env.def_string_offset(&crit).map(|o| o as i32).unwrap_or(-1);
                        f1.push(SoundMapF1Entry { key, value: DefString(off) });
                    }
                    _ => {}
                }
            }
            f0.sort_by_key(|e| e.key);
            f1.sort_by_key(|e| e.key);
            let (mut lowered, warnings) = lower_generic::<EntitySoundDef>(&base, &rest, symbols, env)?;
            lowered.sound_map.f0 = f0;
            lowered.sound_map.f1 = f1;
            (DefBody::EntitySoundDef(lowered), warnings)
        }
        "CFlammableDef" => {
            let base = match base {
                Some(DefBody::FlammableDef(b)) => b.clone(),
                _ => FlammableDef::def_default(),
            };
            let mut entries = base.effect_creation_set.containment_map.0.clone();
            let eval = Evaluator::new(symbols);
            let (matched, rest) = partition_method_calls(body, "EffectCreationSet", "Add");
            for stmt in matched {
                let Statement::MethodCall(mc) = &stmt.value else { continue; };
                let a = &mc.call.arguments;
                let Some(key_str) = eval.arg_string_opt(a, 0) else { continue; };
                let Some(value) = a.get(1).and_then(|e| eval.i32(e).ok()) else { continue; };
                let off = env.def_string_offset(&key_str).map(|o| o as i32).unwrap_or(-1);
                entries.push((DefString(off), value));
            }
            entries.sort_by_key(|(k, _)| k.0);
            let (mut lowered, warnings) = lower_generic::<FlammableDef>(&base, &rest, symbols, env)?;
            lowered.effect_creation_set.containment_map = VecMap(entries);
            (DefBody::FlammableDef(lowered), warnings)
        }
        "CHeroMorphDef" => {
            let base = match base {
                Some(DefBody::HeroMorphDef(b)) => b.clone(),
                _ => HeroMorphDef::def_default(),
            };
            // `TextureMorphs.Add(count, morph_type, replacing_tex, bank_index, blend)`
            // and `ParticleMorphs.Add(morph_type, particle_idx, start_val, end_val,
            // location, loc_name, loc_idx)` / `.AddBlend(morph_type, particle_min,
            // particle_max, start_val, end_val, location, loc_name, loc_idx)`
            use defs::def::text::{PathSegment, Statement};
            let eval = Evaluator::new(symbols);
            let mut tex_morphs = base.texture_morphs.morphs.clone();
            let mut part_morphs = base.particle_morphs.morphs.clone();
            let mut idle_part_morphs = base.idle_particle_morphs.morphs.clone();
            let mut filtered: Vec<Spanned<Statement>> = Vec::new();
            for stmt in body.iter() {
                if let Statement::MethodCall(mc) = &stmt.value
                    && mc.object.segments.len() == 1
                {
                    let fname = match &mc.object.segments[0] {
                        PathSegment::Field(n) => n.as_str(),
                        _ => "",
                    };
                    let a = &mc.call.arguments;
                    match (fname, mc.call.name.as_str()) {
                        ("TextureMorphs", "Add") => {
                            // Add(TextureLayer/count, MorphType, ReplacingTextureIndex, BankIndex, Blend)
                            let morph_type = anim_arg_i32(a, &eval, 1);
                            tex_morphs.push(TextureMorphsMorphsEntry {
                                morph_type,
                                value_morph_type: morph_type,
                                texture_layer: anim_arg_i32(a, &eval, 0),
                                replacing_texture_index: anim_arg_i32(a, &eval, 2),
                                bank_index: anim_arg_i32(a, &eval, 3),
                                blend: anim_arg_i32(a, &eval, 4),
                            });
                            continue;
                        }
                        ("ParticleMorphs", "Add") => {
                            let morph_type = anim_arg_i32(a, &eval, 0);
                            let mut data = [0u8; 36];
                            data[0x00..0x04].copy_from_slice(&morph_type.to_le_bytes());
                            data[0x04..0x08].copy_from_slice(&0i32.to_le_bytes());  // Blending=false
                            data[0x08..0x0c].copy_from_slice(&anim_arg_i32(a, &eval, 4).to_le_bytes());  // Location
                            data[0x0c..0x10].copy_from_slice(&anim_arg_defstring(a, &eval, env, 5).to_le_bytes()); // LocationName
                            data[0x10..0x14].copy_from_slice(&anim_arg_i32(a, &eval, 6).to_le_bytes());  // LocationIndex
                            data[0x14..0x18].copy_from_slice(&anim_arg_i32(a, &eval, 1).to_le_bytes());  // ParticleIndex
                            data[0x18..0x1c].copy_from_slice(&0i32.to_le_bytes());  // SecondParticleIndex
                            data[0x1c..0x20].copy_from_slice(&anim_arg_f32(a, &eval, 2).to_le_bytes());   // StartAtMorphStrength
                            data[0x20..0x24].copy_from_slice(&anim_arg_f32(a, &eval, 3).to_le_bytes());   // EndAtMorphStrength
                            part_morphs.push(ParticleMorphsMorphsEntry { morph_type, data });
                            continue;
                        }
                        ("ParticleMorphs", "AddBlend") => {
                            let morph_type = anim_arg_i32(a, &eval, 0);
                            let mut data = [0u8; 36];
                            data[0x00..0x04].copy_from_slice(&morph_type.to_le_bytes());
                            data[0x04..0x08].copy_from_slice(&1i32.to_le_bytes());  // Blending=true
                            data[0x08..0x0c].copy_from_slice(&anim_arg_i32(a, &eval, 5).to_le_bytes());  // Location
                            data[0x0c..0x10].copy_from_slice(&anim_arg_defstring(a, &eval, env, 6).to_le_bytes()); // LocationName
                            data[0x10..0x14].copy_from_slice(&anim_arg_i32(a, &eval, 7).to_le_bytes());  // LocationIndex
                            data[0x14..0x18].copy_from_slice(&anim_arg_i32(a, &eval, 1).to_le_bytes());  // ParticleIndex (min)
                            data[0x18..0x1c].copy_from_slice(&anim_arg_i32(a, &eval, 2).to_le_bytes());  // SecondParticleIndex (max)
                            data[0x1c..0x20].copy_from_slice(&anim_arg_f32(a, &eval, 3).to_le_bytes());   // StartAtMorphStrength
                            data[0x20..0x24].copy_from_slice(&anim_arg_f32(a, &eval, 4).to_le_bytes());   // EndAtMorphStrength
                            part_morphs.push(ParticleMorphsMorphsEntry { morph_type, data });
                            continue;
                        }
                        ("IdleParticleMorphs", "Add") => {
                            let morph_type = anim_arg_i32(a, &eval, 0);
                            let mut data = [0u8; 36];
                            data[0x00..0x04].copy_from_slice(&morph_type.to_le_bytes());
                            data[0x04..0x08].copy_from_slice(&0i32.to_le_bytes());
                            data[0x08..0x0c].copy_from_slice(&anim_arg_i32(a, &eval, 4).to_le_bytes());  // Location
                            data[0x0c..0x10].copy_from_slice(&anim_arg_defstring(a, &eval, env, 5).to_le_bytes()); // LocationName
                            data[0x10..0x14].copy_from_slice(&anim_arg_i32(a, &eval, 6).to_le_bytes());  // LocationIndex
                            data[0x14..0x18].copy_from_slice(&anim_arg_i32(a, &eval, 1).to_le_bytes());  // ParticleIndex
                            data[0x18..0x1c].copy_from_slice(&0i32.to_le_bytes());
                            data[0x1c..0x20].copy_from_slice(&anim_arg_f32(a, &eval, 2).to_le_bytes());
                            data[0x20..0x24].copy_from_slice(&anim_arg_f32(a, &eval, 3).to_le_bytes());
                            idle_part_morphs.push(ParticleMorphsMorphsEntry { morph_type, data });
                            continue;
                        }
                        ("IdleParticleMorphs", "AddBlend") => {
                            let morph_type = anim_arg_i32(a, &eval, 0);
                            let mut data = [0u8; 36];
                            data[0x00..0x04].copy_from_slice(&morph_type.to_le_bytes());
                            data[0x04..0x08].copy_from_slice(&1i32.to_le_bytes());  // Blending=true
                            data[0x08..0x0c].copy_from_slice(&anim_arg_i32(a, &eval, 5).to_le_bytes());  // Location
                            data[0x0c..0x10].copy_from_slice(&anim_arg_defstring(a, &eval, env, 6).to_le_bytes()); // LocationName
                            data[0x10..0x14].copy_from_slice(&anim_arg_i32(a, &eval, 7).to_le_bytes());  // LocationIndex
                            data[0x14..0x18].copy_from_slice(&anim_arg_i32(a, &eval, 1).to_le_bytes());  // ParticleIndex (min)
                            data[0x18..0x1c].copy_from_slice(&anim_arg_i32(a, &eval, 2).to_le_bytes());  // SecondParticleIndex (max)
                            data[0x1c..0x20].copy_from_slice(&anim_arg_f32(a, &eval, 3).to_le_bytes());
                            data[0x20..0x24].copy_from_slice(&anim_arg_f32(a, &eval, 4).to_le_bytes());
                            idle_part_morphs.push(ParticleMorphsMorphsEntry { morph_type, data });
                            continue;
                        }
                        _ => {}
                    }
                }
                filtered.push(stmt.clone());
            }
            let (mut lowered, warnings) = lower_generic::<HeroMorphDef>(&base, &filtered, symbols, env)?;
            lowered.texture_morphs.morphs = tex_morphs;
            lowered.particle_morphs.morphs = part_morphs;
            lowered.idle_particle_morphs.morphs = idle_part_morphs;
            (DefBody::HeroMorphDef(lowered), warnings)
        }
        "CLookDef" => lower_look_def(base, body, symbols, env)?,
        "OPINION_DEED_EFFECTS" => {
            let base = match base {
                Some(DefBody::OpinionDeedEffectsDef(b)) => b.clone(),
                _ => OpinionDeedEffectsDef::def_default(),
            };
            // `Effects.Add(opinion, peak, run_in_secs, run_out_secs, persist)` is
            // NOT stored verbatim: the C++ ctor (`COpinionTransientOffset`,
            // tc_opinion_of_hero.cpp:4438) converts seconds→frames (×15, the
            // opinion tick rate) and derives per-frame offset rates. Intercept
            // these calls, apply the transform, and strip them before generic
            // lowering (mirrors the OPINION_PERSONALITY arm).
            let mut effects = base.effects.f0.clone();
            let mut filtered_body: Vec<Spanned<defs::def::text::Statement>> = Vec::new();
            for stmt in body.iter() {
                if let defs::def::text::Statement::MethodCall(mc) = &stmt.value
                    && mc.object.segments.len() == 1
                        && matches!(&mc.object.segments[0], PathSegment::Field(n) if n == "Effects")
                        && mc.call.name == "Add"
                    {
                        let eval = Evaluator::new(symbols);
                        let a = &mc.call.arguments;
                        let opinion = a.first().and_then(|e| eval.i32(e).ok()).unwrap_or(0);
                        // 5-arg form (peak, run_in_secs, run_out_secs, persist)
                        // or 2-arg persist-only form (axis, persist).
                        let (peak, run_in_secs, run_out_secs, persist) = if a.len() >= 5 {
                            (
                                eval.f32(&a[1]).unwrap_or(0.0),
                                eval.f32(&a[2]).unwrap_or(0.0),
                                eval.f32(&a[3]).unwrap_or(0.0),
                                eval.f32(&a[4]).unwrap_or(0.0),
                            )
                        } else {
                            (0.0, 0.0, 0.0, eval.f32(&a[1]).unwrap_or(0.0))
                        };
                        effects.push(opinion_transient_offset(
                            opinion, peak, run_in_secs, run_out_secs, persist,
                        ));
                        continue;
                    }
                filtered_body.push(stmt.clone());
            }
            let (mut lowered, warnings) =
                lower_generic::<OpinionDeedEffectsDef>(&base, &filtered_body, symbols, env)?;
            lowered.effects = OpinionTransientOffsetList { f0: effects };
            (DefBody::OpinionDeedEffectsDef(lowered), warnings)
        }
        "OPINION_PERSONALITY" => {
            let base = match base {
                Some(DefBody::OpinionPersonalityDef(b)) => b.clone(),
                _ => OpinionPersonalityDef::def_default(),
            };
            // PersonalityTraits.Set(index, 9 floats) — 5 opinion entries × 36
            // bytes each = 180 bytes.  Extract these calls from the body before
            // generic lowering so `finish()` doesn't reject them as unconsumed.
            let mut traits_blob = base.personality_traits.f0;
            let mut filtered_body: Vec<Spanned<defs::def::text::Statement>> = Vec::new();
            for stmt in body.iter() {
                if let defs::def::text::Statement::MethodCall(mc) = &stmt.value
                    && mc.object.segments.len() == 1
                        && matches!(&mc.object.segments[0], PathSegment::Field(n) if n == "PersonalityTraits")
                        && mc.call.name == "Set"
                    {
                        let eval = Evaluator::new(symbols);
                        let Some(index_expr) = mc.call.arguments.first() else { continue; };
                        let Ok(index) = eval.i32(index_expr) else { continue; };
                        if !(0..5).contains(&index) { continue; }
                        let base = (index as usize) * 36;
                        // Text `Set` arg order is (BaseOffset, HeroStatScaling,
                        // VillageOpinionScaling, TimeScalingRunIn,
                        // TimeScalingRunOut, PeakScalingIfPos,
                        // PersistScalingIfPos, PeakScalingIfNeg,
                        // PersistScalingIfNeg) but the wire/struct order
                        // (tc_opinion_of_hero.hpp:894) groups the Peak fields
                        // then the Persist fields — so text args 6 and 7 swap
                        // into wire slots 7 and 6.
                        const WIRE_SLOT: [usize; 9] = [0, 1, 2, 3, 4, 5, 7, 6, 8];
                        for (fld, &slot) in WIRE_SLOT.iter().enumerate() {
                            let Some(arg) = mc.call.arguments.get(fld + 1) else { break; };
                            let Ok(val) = eval.f32(arg) else { break; };
                            let bytes = val.to_le_bytes();
                            traits_blob[base + slot * 4..base + slot * 4 + 4].copy_from_slice(&bytes);
                        }
                        continue;
                    }
                filtered_body.push(stmt.clone());
            }
            let (mut lowered, warnings) =
                lower_generic::<OpinionPersonalityDef>(&base, &filtered_body, symbols, env)?;
            lowered.personality_traits = OpinionPersonalityTraitsPtr { f0: traits_blob };
            (DefBody::OpinionPersonalityDef(lowered), warnings)
        }
        "OPINION_REACTION_MANAGER" => {
            let base = match base {
                Some(DefBody::OpinionReactionManagerDef(b)) => b.clone(),
                _ => OpinionReactionManagerDef::def_default(),
            };
            let eval = Evaluator::new(symbols);
            const CONSTANT_FPS: f32 = 30.0;

            let mut matches_entries: Vec<ReactionMatchListElementsEntry> = Vec::new();
            let mut freq_traits: [ReactionFrequencyTraitsArrayTraitsEntry; 158] =
                std::array::from_fn(|_| DefDefault::def_default());
            let mut filtered_body: Vec<Spanned<defs::def::text::Statement>> = Vec::new();

            for stmt in body.iter() {
                if let defs::def::text::Statement::MethodCall(mc) = &stmt.value {
                    // Matches.Add(reaction_type, Ctor(...))
                    if mc.object.segments.len() == 1
                        && matches!(&mc.object.segments[0], PathSegment::Field(n) if n == "Matches")
                        && mc.call.name == "Add"
                    {
                        let a = &mc.call.arguments;
                        let reaction_type = a.first().and_then(|e| eval.i32(e).ok()).unwrap_or(0);
                        if let Some(se) = a.get(1)
                            && let Expr::Constructor(ctor) = &se.value {
                            let entry = match ctor.name.as_str() {
                                "Secondary" => {
                                    let axis = anim_arg_i32(&ctor.arguments, &eval, 0);
                                    let lower = anim_arg_f32(&ctor.arguments, &eval, 1) as f64;
                                    let upper = anim_arg_f32(&ctor.arguments, &eval, 2) as f64;
                                    let inv = (1.0 / (upper - lower)) as f32;
                                    ReactionMatchListElementsEntry::Tag0 {
                                        reaction_type,
                                        axis,
                                        lower_bound_on_axis: lower as f32,
                                        inv_interval_on_axis: inv,
                                        m_shift_zero: anim_arg_f32(&ctor.arguments, &eval, 3),
                                        m_shift_weight: anim_arg_f32(&ctor.arguments, &eval, 4),
                                        r_shift_zero: anim_arg_f32(&ctor.arguments, &eval, 5),
                                        r_shift_weight: anim_arg_f32(&ctor.arguments, &eval, 6),
                                    }
                                }
                                "SecondaryCentred" => {
                                    ReactionMatchListElementsEntry::Tag1 {
                                        reaction_type,
                                        axis: anim_arg_i32(&ctor.arguments, &eval, 0),
                                        centre_on_axis: anim_arg_f32(&ctor.arguments, &eval, 1),
                                        radius_on_axis: anim_arg_f32(&ctor.arguments, &eval, 2),
                                        m_shift_zero: anim_arg_f32(&ctor.arguments, &eval, 3),
                                        m_shift_weight: anim_arg_f32(&ctor.arguments, &eval, 4),
                                        r_shift_zero: anim_arg_f32(&ctor.arguments, &eval, 5),
                                        r_shift_weight: anim_arg_f32(&ctor.arguments, &eval, 6),
                                    }
                                }
                                "Pyramid" => {
                                    let x_axis = anim_arg_i32(&ctor.arguments, &eval, 0);
                                    let x_min = anim_arg_f32(&ctor.arguments, &eval, 1) as f64;
                                    let x_centre = anim_arg_f32(&ctor.arguments, &eval, 2) as f64;
                                    let x_max = anim_arg_f32(&ctor.arguments, &eval, 3) as f64;
                                    let y_axis = anim_arg_i32(&ctor.arguments, &eval, 4);
                                    let y_min = anim_arg_f32(&ctor.arguments, &eval, 5) as f64;
                                    let y_centre = anim_arg_f32(&ctor.arguments, &eval, 6) as f64;
                                    let y_max = anim_arg_f32(&ctor.arguments, &eval, 7) as f64;
                                    ReactionMatchListElementsEntry::Tag2 {
                                        reaction_type,
                                        x_axis_opinion_type: x_axis,
                                        y_axis_opinion_type: y_axis,
                                        x_centre: x_centre as f32,
                                        neg_inv_x_radius: (1.0 / (x_centre - x_min)) as f32,
                                        pos_inv_x_radius: (1.0 / (x_max - x_centre)) as f32,
                                        y_centre: y_centre as f32,
                                        neg_inv_y_radius: (1.0 / (y_centre - y_min)) as f32,
                                        pos_inv_y_radius: (1.0 / (y_max - y_centre)) as f32,
                                    }
                                }
                                "MRBlob" => {
                                    let renown_min = anim_arg_f32(&ctor.arguments, &eval, 0) as f64;
                                    let renown_max = anim_arg_f32(&ctor.arguments, &eval, 1) as f64;
                                    let morality_min = anim_arg_f32(&ctor.arguments, &eval, 2) as f64;
                                    let morality_max = anim_arg_f32(&ctor.arguments, &eval, 3) as f64;
                                    let x_centre = morality_min + (morality_max - morality_min) * 0.5;
                                    let y_centre = renown_min + (renown_max - renown_min) * 0.5;
                                    ReactionMatchListElementsEntry::Tag3 {
                                        reaction_type,
                                        x_axis_opinion_type: 0,
                                        y_axis_opinion_type: 1,
                                        x_centre: x_centre as f32,
                                        neg_inv_x_radius: (1.0 / (x_centre - morality_min)) as f32,
                                        pos_inv_x_radius: (1.0 / (morality_max - x_centre)) as f32,
                                        y_centre: y_centre as f32,
                                        neg_inv_y_radius: (1.0 / (y_centre - renown_min)) as f32,
                                        pos_inv_y_radius: (1.0 / (renown_max - y_centre)) as f32,
                                        scariness_shift: anim_arg_f32(&ctor.arguments, &eval, 4),
                                        agreeableness_shift: anim_arg_f32(&ctor.arguments, &eval, 5),
                                        attractiveness_shift: anim_arg_f32(&ctor.arguments, &eval, 6),
                                    }
                                }
                                _ => continue,
                            };
                            matches_entries.push(entry);
                            continue;
                        }
                    }
                    // Frequency.Set(reaction_type, Ctor(...))
                    if mc.object.segments.len() == 1
                        && matches!(&mc.object.segments[0], PathSegment::Field(n) if n == "Frequency")
                        && mc.call.name == "Set"
                    {
                        let a = &mc.call.arguments;
                        let reaction_type = a.first().and_then(|e| eval.i32(e).ok()).unwrap_or(0);
                        if let Some(se) = a.get(1)
                            && let Expr::Constructor(ctor) = &se.value {
                            let idx = reaction_type as usize;
                            if idx < 79 {
                                match ctor.name.as_str() {
                                    "Linear" => {
                                        let min_wait = anim_arg_f32(&ctor.arguments, &eval, 0);
                                        let max_wait = anim_arg_f32(&ctor.arguments, &eval, 1);
                                        let wait_range = (max_wait as f64 - min_wait as f64) as f32;
                                        freq_traits[idx] = ReactionFrequencyTraitsArrayTraitsEntry::Tag1 {
                                            min_wait,
                                            wait_range,
                                        };
                                        let h_min = (CONSTANT_FPS as f64 * 0.2 * min_wait as f64) as f32;
                                        let h_max = (CONSTANT_FPS as f64 * 0.2 * max_wait as f64) as f32;
                                        let h_range = (h_max as f64 - h_min as f64) as f32;
                                        freq_traits[idx + 79] = ReactionFrequencyTraitsArrayTraitsEntry::Tag1 {
                                            min_wait: h_min,
                                            wait_range: h_range,
                                        };
                                    }
                                    "ControlledCount" => {
                                        let max_count = anim_arg_f32(&ctor.arguments, &eval, 0);
                                        let min_gap = anim_arg_f32(&ctor.arguments, &eval, 1);
                                        let recharge_time = anim_arg_f32(&ctor.arguments, &eval, 2);
                                        let allow_repeats = opinion_arg_bool(&ctor.arguments, &eval, 3);
                                        freq_traits[idx] = controlled_count_trait(max_count, min_gap, recharge_time, allow_repeats, CONSTANT_FPS);
                                        freq_traits[idx + 79] = controlled_count_trait(max_count * 5.0, min_gap * 0.2, recharge_time * 0.2, allow_repeats, CONSTANT_FPS);
                                    }
                                    _ => {}
                                }
                            }
                            continue;
                        }
                    }
                }
                filtered_body.push(stmt.clone());
            }

            let (mut lowered, warnings) =
                lower_generic::<OpinionReactionManagerDef>(&base, &filtered_body, symbols, env)?;
            // C++ std::map fields — VecMap stores in text declaration order;
            // sort by key to match retail's std::map sorted order.
            lowered.attitude_condition.0.sort_by_key(|(k, _)| k.get_i32());
            lowered.targeting_condition.0.sort_by_key(|(k, _)| k.get_i32());
            lowered.pre_reaction_delay.0.sort_by_key(|(k, _)| k.get_i32());
            lowered.tolerance_to_being_hit.0.sort_by_key(|(k, _)| k.get_i32());
            lowered.block_further_reactions.0.sort_by_key(|(k, _)| k.get_i32());
            lowered.allow_speech_on_non_pure_ai_speaker.0.sort_by_key(|(k, _)| k.get_i32());
            lowered.allow_while_carrying.0.sort_by_key(|(k, _)| k.get_i32());
            lowered.allow_while_following_player.0.sort_by_key(|(k, _)| k.get_i32());
            lowered.matches = ReactionMatchList { elements: matches_entries };
            lowered.frequency = ReactionFrequencyTraitsArray { traits: freq_traits };
            (DefBody::OpinionReactionManagerDef(lowered), warnings)
        }
        "OPINION_SOURCE" => {
            let base = match base {
                Some(DefBody::OpinionSourceDef(b)) => b.clone(),
                _ => OpinionSourceDef::def_default(),
            };
            let (mut lowered, warnings) = lower_generic(&base, body, symbols, env)?;
            // The 79 BinaryReaction bools are derived from ReactionFlagDefault
            // + the ReactionFlag map, not read as controls.
            lowered.derive_binary_reactions();
            lowered.derive_binary_opinions();
            (DefBody::OpinionSourceDef(lowered), warnings)
        }
        "PLAYER_GUI" => {
            // The Highlight* Vec fields are pre-sized with `.resize(N)` and then
            // filled by indexed constructors/scalars
            // (`HighlightStartColour[CURSOR_TYPE_USABLE] CRGBColour(...)`), all
            // handled by the generic vec/constructor-in-group lowering.
            let base = match base {
                Some(DefBody::PlayerGuiDef(b)) => b.clone(),
                _ => PlayerGuiDef::def_default(),
            };
            let (mut lowered, warnings) =
                lower_generic::<PlayerGuiDef>(&base, body, symbols, env)?;
            // VecMap<String, i32> fields must be sorted by CRC32 of the key to
            // match C++ std::map<CCharString, long> ordering.
            let sort_by_crc32 = |v: &mut VecMap<String, i32>| {
                v.0.sort_by(|(a, _), (b, _)| {
                    crc32::crc(a.as_bytes())
                        .cmp(&crc32::crc(b.as_bytes()))
                });
            };
            sort_by_crc32(&mut lowered.script_sprites);
            sort_by_crc32(&mut lowered.game_action_values);
            sort_by_crc32(&mut lowered.mini_map_graphics.f0);
            (DefBody::PlayerGuiDef(lowered), warnings)
        }
        "CSpecialEffectsDef" => {
            let base = match base {
                Some(DefBody::SpecialEffectsDef(b)) => b.clone(),
                _ => SpecialEffectsDef::def_default(),
            };
            let mut entries = base.special_effects.f0.0.clone();
            let eval = Evaluator::new(symbols);
            let (matched, rest) = partition_method_calls(body, "SpecialEffects", "Add");
            for stmt in matched {
                let Statement::MethodCall(mc) = &stmt.value else { continue; };
                let a = &mc.call.arguments;
                let Some(k) = eval.arg_string_opt(a, 0) else { continue; };
                let Some(v) = a.get(1).and_then(|e| eval.i32(e).ok()) else { continue; };
                entries.push((crc32::crc(k.as_bytes()), v));
            }
            let mut seen = std::collections::HashSet::new();
            entries.retain(|(k, _)| seen.insert(*k));
            entries.sort_by_key(|(k, _)| *k);
            let (mut lowered, warnings) = lower_generic::<SpecialEffectsDef>(&base, &rest, symbols, env)?;
            lowered.special_effects.f0 = VecMap(entries);
            (DefBody::SpecialEffectsDef(lowered), warnings)
        }
        // Every Thing-family def lowers identically: the generic field walk
        // plus the component set rebuilt from `Components.Add/Remove` calls.
        "THING" => lower_thing!(base, body, symbols, env, ThingBaseDef),
        "BUILDING" => lower_thing!(base, body, symbols, env, ThingBuildingDef),
        "CREATURE" => lower_thing!(base, body, symbols, env, ThingCreatureDef),
        "HOLY_SITE" => lower_thing!(base, body, symbols, env, ThingHolySiteDef),
        "MARKER" => lower_thing!(base, body, symbols, env, ThingMarkerDef),
        "NOISE" => lower_thing!(base, body, symbols, env, ThingNoiseDef),
        "OBJECT" => lower_thing!(base, body, symbols, env, ThingObjectDef),
        "PHYSICAL_SWITCH" => lower_thing!(base, body, symbols, env, ThingPhysicalSwitchDef),
        "SWITCH" => lower_thing!(base, body, symbols, env, ThingSwitchDef),
        "VILLAGE" => lower_thing!(base, body, symbols, env, ThingVillageDef),
        "CWeaponDef" => {
            let base = match base {
                Some(DefBody::WeaponDef(b)) => b.clone(),
                _ => WeaponDef::def_default(),
            };
            // `WeaponTrails[augmentation] CWeaponTrailGraphicSet(attack, knockdown)`
            // is stored SWAPPED on the wire: the map is keyed by the trail-graphic
            // set with the augmentation flags as the VALUE
            // (`std::map<CWeaponTrailGraphicSet, EObjectAugmentationType>`, verified
            // against retail). The generic keyed applier would try to build the
            // struct key from the augmentation symbol and fail. Intercept and
            // build the swapped entries, then strip before generic lowering.
            use defs::def::text::{PathSegment, Statement};
            let mut trails = base.weapon_trails.0.clone();
            let mut filtered: Vec<Spanned<Statement>> = Vec::new();
            for stmt in body.iter() {
                if let Statement::Field(f) = &stmt.value
                    && f.path.segments.len() == 2
                    && matches!(&f.path.segments[0], PathSegment::Field(n) if n == "WeaponTrails")
                    && let PathSegment::Index(aug_expr) = &f.path.segments[1]
                    && let Expr::Constructor(c) = &f.expr.value
                {
                    let eval = Evaluator::new(symbols);
                    // Wire triple is [augmentation, graphic0, graphic1], which our
                    // model reads as (WeaponTrailGraphicSet{attack,knockdown}, value):
                    // key.attack = augmentation, key.knockdown = ctor arg0,
                    // value = ctor arg1 (verified against retail).
                    let aug = eval.i32(aug_expr).unwrap_or(0);
                    let g0 = c.arguments.first().and_then(|e| eval.i32(e).ok()).unwrap_or(0);
                    let g1 = c.arguments.get(1).and_then(|e| eval.i32(e).ok()).unwrap_or(0);
                    let key = WeaponTrailGraphicSet { attack: aug, knockdown: g0 };
                    let val = ObjectAugmentationType::from_i32(g1);
                    // Last-wins dedup by augmentation type (key.attack):
                    // the C++ Transfer keyed on EObjectAugmentationType
                    // before swapping to the wire. Verified against
                    // retail (CREATURE_DRAGON_BASE drops parent's
                    // AUGMENTATION_NULL entry in favour of its own).
                    if let Some(slot) = trails.iter_mut().find(|(k, _)| k.attack == key.attack) {
                        *slot = (key, val);
                    } else {
                        trails.push((key, val));
                    }
                    continue;
                }
                filtered.push(stmt.clone());
            }
            // std::map<CWeaponTrailGraphicSet, …> → key-sorted (attack, knockdown).
            trails.sort_by_key(|(k, _)| (k.attack, k.knockdown));
            let (mut lowered, warnings) = lower_generic::<WeaponDef>(&base, &filtered, symbols, env)?;
            lowered.weapon_trails = VecMap(trails);
            (DefBody::WeaponDef(lowered), warnings)
        }
        "CWillResponseDef" => lower_will_response_def(base, body, symbols, env)?,
        "CDegradableDef" => {
            let base = match base {
                Some(DefBody::DegradableDef(b)) => b.clone(),
                _ => DegradableDef::def_default(),
            };
            let eval = Evaluator::new(symbols);
            let mut degradations = base.degradations.clone();
            let (matched, rest) = partition_method_calls(body, "Degradations", "Add");
            for stmt in matched {
                let Statement::MethodCall(mc) = &stmt.value else { continue; };
                let a = &mc.call.arguments;
                let mut entry = DegradableInfo::def_default();
                entry.health_percentage = eval.arg_f32_or(a, 0, 0.0);
                entry.bank_index = eval.arg_i32_or(a, 1, 0);
                entry.smash_particle_emitter = eval.arg_i32_or(a, 2, 0);
                entry.blocks_navigation = eval.arg_bool_or(a, 3, false);
                degradations.push(entry);
            }
            let (mut lowered, warnings) = lower_generic::<DegradableDef>(&base, &rest, symbols, env)?;
            let gtype = lowered.graphic_type.to_i32() as u8;
            for d in degradations.iter_mut() {
                d.type_ = gtype;
            }
            lowered.degradations = degradations;
            (DefBody::DegradableDef(lowered), warnings)
        }
        "CReplaceableMeshDef" => {
            let base = match base {
                Some(DefBody::ReplaceableMeshDef(b)) => b.clone(),
                _ => ReplaceableMeshDef::def_default(),
            };
            let mut meshes = base.meshes.vector.clone();
            let eval = Evaluator::new(symbols);
            let (matched, rest) = partition_method_calls(body, "Meshes", "Add");
            for stmt in matched {
                let Statement::MethodCall(mc) = &stmt.value else { continue; };
                let a = &mc.call.arguments;
                meshes.push(ReplaceableMeshesEntry {
                    bank_index: eval.arg_i32_or(a, 1, 0),
                    anim_step: 1.0,
                    render_size_x: 1.0,
                    additive_alpha: 0,
                    graphic_type: eval.arg_i32_or(a, 0, 0) as u8,
                });
            }
            let (mut lowered, warnings) = lower_generic::<ReplaceableMeshDef>(&base, &rest, symbols, env)?;
            lowered.meshes.vector = meshes;
            (DefBody::ReplaceableMeshDef(lowered), warnings)
        }
        "CAppearanceModifierDef" => {
            use defs::def::values::AppearanceModifierGraphicsGraphicsEntry;
            let base = match base {
                Some(DefBody::AppearanceModifierDef(b)) => b.clone(),
                _ => AppearanceModifierDef::def_default(),
            };
            let mut graphics = base.graphics.graphics.clone();
            let eval = Evaluator::new(symbols);
            let (matched, rest) = partition_method_calls(body, "Graphics", "Add");
            for stmt in matched {
                let Statement::MethodCall(mc) = &stmt.value else { continue; };
                let a = &mc.call.arguments;
                let mut data = [0u8; 24];
                data[0x00..0x04].copy_from_slice(&eval.arg_i32_or(a, 0, 0).to_le_bytes());
                data[0x04..0x08].copy_from_slice(&eval.arg_i32_or(a, 3, 0).to_le_bytes());
                data[0x08..0x0c].copy_from_slice(&eval.arg_i32_or(a, 1, 0).to_le_bytes());
                data[0x0c..0x10].copy_from_slice(&eval.arg_f32_or(a, 2, 0.0).to_le_bytes());
                data[0x10..0x14].copy_from_slice(&eval.arg_f32_or(a, 4, 0.0).to_le_bytes());
                data[0x14..0x18].copy_from_slice(&eval.arg_f32_or(a, 5, 0.0).to_le_bytes());
                graphics.push(AppearanceModifierGraphicsGraphicsEntry { data });
            }
            let (mut lowered, warnings) = lower_generic::<AppearanceModifierDef>(&base, &rest, symbols, env)?;
            lowered.graphics.graphics = graphics;
            (DefBody::AppearanceModifierDef(lowered), warnings)
        }
        "CONTROL_SCHEME" => {
            let base = match base {
                Some(DefBody::Controls(b)) => b.clone(),
                _ => ControlsDef::def_default(),
            };
            (DefBody::Controls(crate::lower::lower_controls(&base, body, symbols)?), Vec::new())
        }
        "UI" => {
            let base = match base {
                Some(DefBody::Ui(b)) => b.clone(),
                _ => UiDef::def_default(),
            };
            (DefBody::Ui(crate::lower::lower_ui(&base, body, symbols, env)?), Vec::new())
        }
        "UI_MISC_THINGS_DEF" => {
            let base = match base {
                Some(DefBody::UiMiscThings(b)) => b.clone(),
                _ => UiMiscThingsDef::def_default(),
            };
            (DefBody::UiMiscThings(crate::lower::lower_ui_misc_things(&base, body, symbols, env)?), Vec::new())
        }
        "FRONT_END" => {
            let base = match base {
                Some(DefBody::FrontEnd(b)) => b.clone(),
                _ => FrontEndDef::def_default(),
            };
            (DefBody::FrontEnd(crate::lower::lower_front_end(&base, body, symbols)?), Vec::new())
        }
        "CHeroExperienceDef" => lower_hero_experience_def(base, body, symbols, env)?,
        "CScriptDef" => lower_script_def(base, body, symbols, env)?,
        _ => {
            match base {
                Some(b) => lower_generic(b, body, symbols, env)?,
                None => {
                    let default = DefBody::def_default_for_name(name)
                        .ok_or_else(|| LowerError::Unsupported(name.to_string()))?;
                    lower_generic(&default, body, symbols, env)?
                }
            }
        }
    };
    Ok(body)
}
