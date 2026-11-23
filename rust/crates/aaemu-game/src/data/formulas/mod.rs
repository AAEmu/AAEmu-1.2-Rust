//! Port of `FormulaManager` (`AAEmu.Game/Core/Managers/FormulaManager.cs`) and the
//! `AAEmu.Game/Models/Game/Formulas/*` models, plus a re-implementation of the expression engine
//! the C# borrows from the NuGet package **Jace 1.0.0** (`Jace.CalculationEngine`).
//!
//! # Expression engine
//!
//! The C# builds one `CalculationEngine` in `FormulaManager.Load` with `CacheEnabled`,
//! `OptimizerEnabled`, `CaseSensitive = true`, `ExecutionMode.Compiled` and `InvariantCulture`,
//! and registers four extra functions (`clamp`, `if_negative`, `if_positive`, `if_zero`).
//! [`Expression`] reproduces that engine: the formula text is tokenized and turned into an AST once
//! ([`Expression::parse`], used by [`Formula::prepare`]) and evaluated many times.
//!
//! The behaviour below was taken from the IL of the real Jace 1.0.0 assembly (`TokenReader`,
//! `AstBuilder`, `Optimizer`, `MathExtended`) and verified with a throw-away .NET 10 harness
//! configured exactly like `FormulaManager.Load`, diffed bit-for-bit (including the sign of zero
//! and error-vs-value) against this module on ~200k generated well-formed and garbage formulas:
//!
//! * **Tokens.** Only `' '` is whitespace (tab / newline / NBSP are "invalid token" errors).
//!   Numbers are ASCII digits, `.` and an `e`/`E` exponent whose sign may only be `-`
//!   (`1e-2` is fine, `1e+2` is a syntax error). A `-` at the start of the formula or right after
//!   `+ - * / % ^ ≤ ≥ ≠ (` starts a number token (`-2`, but `-e` is then a broken number).
//!   A digit run that is not a valid number (`1.2.3`, `2ex`) is **silently dropped** by Jace's
//!   `TokenReader` ("Else we skip") — mirrored, so `"1.2.3 x"` evaluates to `x`.
//!   Identifiers are ASCII: a letter, then letters, digits or `_`. A `-` after a comparison or
//!   logical operator, `,` or an operand is an operator token (unary or binary), not a literal.
//! * **Operators and precedence** (Jace `AstBuilder`): `&& ||` (1) < `< <= > >= == != ≤ ≥ ≠`
//!   (2) < `+ -` (3) < `* / %` (4) < `^` (5) < unary minus (6). Only `+ - * /` are
//!   left-associative; `% ^`, the comparisons and `&& ||` are right-associative, and the
//!   associativity of the *incoming* operator decides ties, so `2*3%4 = 2*(3%4) = 6`,
//!   `8%3*2 = 4`, `2^3^2 = 512`, `1<2<3 = 1<(2<3) = 0`, `1||0&&0 = 1`. Unary minus binds tighter
//!   than `^`: `-2^2 = 4`, `-x^2 = (-x)^2`, `2^-3^2 = 2^((-3)^2)`. There is no unary plus.
//!   Comparisons and `&&`/`||` yield `1.0`/`0.0`; `&&`/`||` test `!= 0.0` (so NaN is "true") and,
//!   like every function, evaluate **both** operands (no short-circuit).
//! * **Functions** (names are case-*insensitive* even with `CaseSensitive = true`; any identifier
//!   that names a function is a function, `max` alone is a syntax error): `sin cos csc sec asin
//!   acos tan cot atan acot loge log10 logn sqrt abs if ifless ifmore ifequal ceiling floor
//!   truncate round random` (fixed arity), `max min avg median` (one or more arguments), plus the
//!   four `FormulaManager` functions. `round` is banker's rounding (`Math.Round`), `logn(a, b)` is
//!   `Math.Log(a, b)`, `max`/`min`/`avg` follow LINQ (`max` skips NaN, `min` propagates it),
//!   `median` is Jace's quickselect of the lower median (NaN sorts first).
//! * **Variables** are case-sensitive. A variable missing from the parameter map falls back to
//!   the constants `e` / `pi` (matched case-insensitively, so `PI` works but a `pi` parameter
//!   overrides the constant); otherwise evaluation fails with `VariableNotDefinedException`
//!   ([`EvalError::VariableNotDefined`]), which `Formula.Evaluate` turns into `0`.
//! * **Optimizer.** Jace folds every variable-free, deterministic sub-tree into a floating-point
//!   constant, recursing only through `+ - * / ^` and function arguments, and rewrites a
//!   multiplication with a *floating-point* `0.0` constant operand to `0.0`. This is observable
//!   and mirrored: `0.0*x` is `0` even when `x` is missing / NaN / infinite, while `0*x` (integer
//!   literal) and `-(0.0*x)` still fail on a missing `x`.
//! * Arithmetic is IEEE `f64` (`1/0 = inf`, `0/0 = NaN`, `%` is `fmod` like C#).
//! * Malformed input is rejected exactly where Jace rejects it, including Jace's shunting-yard
//!   leniencies (`"1 2 +"` is `3`, `"sin 1"` is `sin(1)`, `"2 max()"` is `max(2)`).
//!
//! # Deviations (documented, deliberate)
//!
//! * Error messages are close to, but not byte-identical with, Jace's exceptions (some of Jace's
//!   failures are raw `IndexOutOfRangeException` / `InvalidOperationException`s).
//! * Error positions count Unicode scalars, Jace counts UTF-16 units (only differs after a
//!   character outside the BMP, which is an invalid token anyway).
//! * `random()` uses a small thread-local SplitMix64 generator instead of `System.Random`.
//! * [`Formula::evaluate`] on a formula whose [`Formula::prepare`] failed logs and returns `0`;
//!   the C# would throw `ArgumentNullException` out of `lock (Expression)` (see
//!   [`Formula::evaluate`]).
//! * The C# `_loaded` static guard is not ported; [`FormulaManager::load`] builds a fresh value.

use std::borrow::Borrow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::hash::{BuildHasher, Hash};
use std::ops::Deref;
use std::sync::{Mutex, OnceLock, PoisonError};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, Row};

mod expression;
pub use expression::{EvalError, Expression, ParseError};

// ---------------------------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------------------------

/// Generates a C#-style enum with its exact discriminants, a checked conversion from the raw
/// value (C# casts any raw value into the enum, so callers keep the raw value when this fails)
/// and the conversion back.
macro_rules! raw_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident : $repr:ident via $from:ident {
            $( $(#[$vmeta:meta])* $variant:ident = $value:literal, )*
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr($repr)]
        $vis enum $name {
            $( $(#[$vmeta])* $variant = $value, )*
        }

        impl $name {
            /// Every defined value (`Enum.GetValues`).
            pub const ALL: &'static [$name] = &[$( $name::$variant, )*];

            /// The variant with this raw value, or `None` for a value the C# enum does not name.
            pub const fn $from(value: $repr) -> Option<Self> {
                match value {
                    $( $value => Some(Self::$variant), )*
                    _ => None,
                }
            }

            /// The raw (wire / database) value.
            pub const fn raw(self) -> $repr {
                self as $repr
            }
        }

        impl From<$name> for $repr {
            fn from(value: $name) -> $repr {
                value as $repr
            }
        }

        impl TryFrom<$repr> for $name {
            type Error = $repr;

            fn try_from(value: $repr) -> Result<Self, $repr> {
                Self::$from(value).ok_or(value)
            }
        }
    };
}

raw_enum! {
    /// `FormulaOwnerType` (`unit_formulas.owner_type_id`).
    pub enum FormulaOwnerType: u8 via from_u8 {
        Character = 0,
        Npc = 1,
        Slave = 2,
        Housing = 3,
        Transfer = 4,
        Mate = 5,
        Shipyard = 6,
    }
}

raw_enum! {
    /// `UnitFormulaKind` (`unit_formulas.kind_id`). Values 3, 4, 8 and 9 (`MeleeDodge`,
    /// `MeleeBlock`, `RangedDodge`, `RangedBlock`) are commented out in the C# ("do not exist").
    pub enum UnitFormulaKind: u8 via from_u8 {
        MeleeCritical = 1,
        MeleeAntiMiss = 2,
        MeleeParry = 5,
        RangedCritical = 6,
        RangedAntiMiss = 7,
        SpellCritical = 10,
        SpellAntiMiss = 11,
        LevelDps = 12,
        LevelMana = 13,
        MaxHealth = 14,
        MaxMana = 15,
        HealthRegen = 16,
        ManaRegen = 17,
        Armor = 18,
        MagicResist = 19,
        Facet = 20,
        MeleeDpsInc = 21,
        Str = 22,
        Dex = 23,
        Sta = 24,
        Int = 25,
        Spi = 26,
        Fai = 27,
        RangedDpsInc = 28,
        SpellDpsInc = 29,
        CastingTolerance = 30,
        PersistentHealthRegen = 31,
        PersistentManaRegen = 32,
        BaseMissPercent = 33,
        KillExp = 34,
        MeleeCriticalBonus = 35,
        RangedCriticalBonus = 36,
        SpellCriticalBonus = 37,
        RangedParry = 38,
        IncomingMeleeDamageAdd = 39,
        IncomingRangedDamageAdd = 40,
        IncomingSpellDamageAdd = 41,
        HealCritical = 42,
        HealDpsInc = 43,
        HealCriticalBonus = 44,
        Block = 45,
        Dodge = 46,
    }
}

raw_enum! {
    /// `UnitFormulaVariableType` (`unit_formula_variables.variable_kind_id`).
    pub enum UnitFormulaVariableType: u8 via from_u8 {
        Unknown = 1,
        NpcTemplate = 2,
        NpcKind = 3,
        NpcGrade = 4,
        MateKind = 5,
    }
}

raw_enum! {
    /// `WearableFormulaType` (`wearable_formulas.kind_id`).
    pub enum WearableFormulaType: u8 via from_u8 {
        MaxBaseArmor = 0,
        MaxBaseMagicResistance = 1,
    }
}

raw_enum! {
    /// `FormulaKind`: ids of rows in the `formulas` table (`FormulaManager.GetFormula((uint)kind)`).
    /// Values from 28 on only exist in versions after 1.2.
    pub enum FormulaKind: u32 via from_u32 {
        CastingCancelPercent = 2,
        CastingDelayTime = 3,
        DetectStealthRangeFront = 4,
        NpcPerceptionRangeMod = 5,
        MoneyForRecoverExp = 6,
        LaborPowerForRecoverExp = 7,
        PenaltyExp = 8,
        RecoverableExp = 9,
        DetectStealthRangeBack = 10,
        DamageMultiplierByHeight = 11,
        DamageMultiplierByRange = 12,
        ArmorGradeBuffABLevel = 13,
        NPCFlyPassDist = 14,
        NPCFlyPassHeight = 15,
        NPCFlyDiveDist = 16,
        NPCFlyDiveHeight = 17,
        NPCFlyQuickTurnDist = 18,
        ExpByLaborPower = 19,
        DamageBonusMultiplierByLevel = 20,
        DamagePenaltyMultiplierByLevel = 21,
        GradeEnchantCost = 22,
        DamageReduceRadioByBattleResist = 23,
        FacetsForBullsEye = 24,
        FlexibilityRatio = 25,
        FlexibilityBonus = 26,
        ExpBySkillEffect = 27,
        PhysicsCollisionDamage = 28,
        SlaveEquipmentGradeEnchantCost = 29,
        GearScoreWeaponArmorAcc = 30,
        GearScoreSocket = 31,
        GearScoreEnchantingGem = 32,
        ConquestBonusScore = 33,
        ConquestPenaltyScore = 34,
        AAPointMaxLimitByLevel = 35,
        LeadershipPointDecreaseForNationChange = 36,
        MateEquipmentEchantCost = 37,
        ItemSocketingCost = 38,
        UnbindEquipBindItem = 39,
        ResetSkillCost = 40,
        SwapAbilityCost = 41,
        SwapAbilitySetCost = 42,
        EloRatingCalculation = 43,
        BlessUthstinConsumeItemNum = 44,
        BlessUthstinExtendMaxStat = 45,
    }
}

// ---------------------------------------------------------------------------------------------
// Formula models
// ---------------------------------------------------------------------------------------------

/// Formulas (id and text) whose evaluation failure has already been logged at `warn` (see
/// [`Formula::evaluate`]); guarded by a `Mutex` that a panic must not poison for later callers.
/// The text is part of the key: every [`Formula::new`] formula has id 0 and the ids of the
/// `formulas`, `unit_formulas` and `wearable_formulas` tables overlap.
static EVAL_FAILURE_WARNED: OnceLock<Mutex<HashMap<u32, HashSet<String>>>> = OnceLock::new();

/// `true` the first time a formula (id and text) fails to evaluate in this process.
fn eval_failure_is_first(id: u32, text_formula: &str) -> bool {
    let mut warned = EVAL_FAILURE_WARNED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let texts = warned.entry(id).or_default();
    !texts.contains(text_formula) && texts.insert(text_formula.to_owned())
}

/// `Formula`: a database formula plus its compiled expression.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Formula {
    /// `Formula.Id` (0 for formulas built with [`Formula::new`]).
    pub id: u32,
    /// `Formula.TextFormula`.
    pub text_formula: String,
    /// `Formula.Expression` (`None` until [`Formula::prepare`] succeeds).
    expression: Option<Expression>,
}

impl Formula {
    /// `new Formula(string formula)`: sets the text and calls `Prepare()` (whose failure is only
    /// logged, as in the C#).
    pub fn new(text_formula: impl Into<String>) -> Formula {
        let mut formula = Formula::unprepared(0, text_formula);
        formula.prepare();
        formula
    }

    /// `new Formula { Id = id, TextFormula = text }` without calling `Prepare()`.
    pub fn unprepared(id: u32, text_formula: impl Into<String>) -> Formula {
        Formula {
            id,
            text_formula: text_formula.into(),
            expression: None,
        }
    }

    /// `Formula.Prepare()`: compiles [`Formula::text_formula`]; on a syntax error logs it and
    /// returns `false`.
    pub fn prepare(&mut self) -> bool {
        match Expression::parse(&self.text_formula) {
            Ok(expression) => {
                self.expression = Some(expression);
                true
            }
            Err(e) => {
                tracing::error!(
                    "Formula {} : {} has syntax errors: ",
                    self.id,
                    self.text_formula
                );
                tracing::error!("{e}");
                false
            }
        }
    }

    /// Whether [`Formula::prepare`] has succeeded.
    pub fn is_prepared(&self) -> bool {
        self.expression.is_some()
    }

    /// The compiled expression, if prepared.
    pub fn expression(&self) -> Option<&Expression> {
        self.expression.as_ref()
    }

    /// `Formula.Evaluate(parameters)`: on any evaluation error (in practice a variable missing
    /// from `params`) returns `0`. The first failure of a formula (by id and text) logs the
    /// formula, the parameters and the error at `warn`; later failures of the same formula log at
    /// `debug`, so a broken row evaluated on every hit cannot flood the log.
    ///
    /// Deviation: the C# would throw `ArgumentNullException` out of `lock (Expression)` for a
    /// formula whose `Prepare()` failed (only possible for `new Formula(text)`, e.g. item
    /// templates' `formula_dps`); here that is logged once and `0` is returned.
    pub fn evaluate<K, S>(&self, params: &HashMap<K, f64, S>) -> f64
    where
        K: Borrow<str> + Eq + Hash,
        S: BuildHasher,
    {
        let Some(expression) = &self.expression else {
            if eval_failure_is_first(self.id, &self.text_formula) {
                tracing::warn!(
                    "Formula {} : {} was never successfully prepared",
                    self.id,
                    self.text_formula
                );
            } else {
                tracing::debug!(
                    "Formula {} : {} was never successfully prepared",
                    self.id,
                    self.text_formula
                );
            }
            return 0.0;
        };
        match expression.evaluate(params) {
            Ok(value) => value,
            Err(e) => {
                if eval_failure_is_first(self.id, &self.text_formula) {
                    let mut sb = String::new();
                    for (key, value) in params {
                        let _ = writeln!(sb, "{}: {}", key.borrow(), value);
                    }
                    tracing::warn!("Error in formula {}:\n{}", self.id, self.text_formula);
                    tracing::warn!("Parameters:\n{}", sb);
                    tracing::warn!("{e}");
                } else {
                    tracing::debug!("Formula {} evaluation failed: {e}", self.id);
                }
                0.0
            }
        }
    }
}

/// `UnitFormula : Formula` (`unit_formulas` row).
#[derive(Debug, Clone, PartialEq)]
pub struct UnitFormula {
    pub formula: Formula,
    /// `UnitFormula.Kind`, raw (`kind_id`); see [`UnitFormula::kind`].
    pub kind_id: u8,
    /// `UnitFormula.Owner`, raw (`owner_type_id`); see [`UnitFormula::owner`].
    pub owner_type_id: u8,
}

impl UnitFormula {
    /// `UnitFormula.Kind` (`None` for a kind the C# enum does not name).
    pub fn kind(&self) -> Option<UnitFormulaKind> {
        UnitFormulaKind::from_u8(self.kind_id)
    }

    /// `UnitFormula.Owner`.
    pub fn owner(&self) -> Option<FormulaOwnerType> {
        FormulaOwnerType::from_u8(self.owner_type_id)
    }
}

impl Deref for UnitFormula {
    type Target = Formula;

    fn deref(&self) -> &Formula {
        &self.formula
    }
}

/// `UnitFormulaVariable` (`unit_formula_variables` row).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UnitFormulaVariable {
    pub formula_id: u32,
    /// `UnitFormulaVariable.Type`, raw (`variable_kind_id`); see [`UnitFormulaVariable::kind`].
    pub variable_kind_id: u8,
    pub key: u32,
    pub value: f32,
}

impl UnitFormulaVariable {
    /// `UnitFormulaVariable.Type`.
    pub fn kind(&self) -> Option<UnitFormulaVariableType> {
        UnitFormulaVariableType::from_u8(self.variable_kind_id)
    }
}

/// `WearableFormula : Formula` (`wearable_formulas` row).
#[derive(Debug, Clone, PartialEq)]
pub struct WearableFormula {
    pub formula: Formula,
    /// `WearableFormula.Type`, raw (`kind_id`); see [`WearableFormula::kind`].
    pub kind_id: u8,
}

impl WearableFormula {
    /// `WearableFormula.Type`.
    pub fn kind(&self) -> Option<WearableFormulaType> {
        WearableFormulaType::from_u8(self.kind_id)
    }
}

impl Deref for WearableFormula {
    type Target = Formula;

    fn deref(&self) -> &Formula {
        &self.formula
    }
}

// ---------------------------------------------------------------------------------------------
// FormulaManager
// ---------------------------------------------------------------------------------------------

/// Failure of [`FormulaManager::load`]: the exceptions `FormulaManager.Load` lets escape.
#[derive(Debug, thiserror::Error)]
pub enum FormulaLoadError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// `SQLiteWrapperReader` getters on a NULL column.
    #[error("{table}.{column} is NULL")]
    NullValue {
        table: &'static str,
        column: &'static str,
    },
    /// `GetByte` / `GetInt32` overflow (`OverflowException`).
    #[error("{table}.{column} value {value} does not fit in {target}")]
    Overflow {
        table: &'static str,
        column: &'static str,
        value: i64,
        target: &'static str,
    },
    /// A column holding a BLOB where a number or text is expected.
    #[error("{table}.{column} has an unsupported storage class")]
    InvalidType {
        table: &'static str,
        column: &'static str,
    },
    /// `_unitFormulas[formula.Owner]` (`KeyNotFoundException`): the outer dictionary only holds
    /// the defined `FormulaOwnerType` values.
    #[error("unit formula {id}: owner_type_id {owner} is not a FormulaOwnerType")]
    UnknownOwner { id: u32, owner: u8 },
    /// `Dictionary.Add` of a duplicate `(owner, kind)` (`ArgumentException`).
    #[error("unit formula {id}: duplicate formula for owner {owner}, kind {kind}")]
    DuplicateUnitFormula { id: u32, owner: u8, kind: u8 },
    /// `Dictionary.Add` of a duplicate `(unit_formula_id, variable_kind_id, key)`.
    #[error("duplicate unit formula variable (formula {formula_id}, type {kind}, key {key})")]
    DuplicateUnitVariable { formula_id: u32, kind: u8, key: u32 },
    /// `Dictionary.Add` of a duplicate wearable formula type.
    #[error("wearable formula {id}: duplicate formula for type {kind}")]
    DuplicateWearableFormula { id: u32, kind: u8 },
    /// `Dictionary.Add` of a duplicate formula id.
    #[error("duplicate formula id {0}")]
    DuplicateFormula(u32),
}

/// `FormulaManager`: unit / wearable / generic formulas and the per-NPC/mate unit variables.
///
/// Maps are keyed by the raw database values so that kinds the C# enums do not name are still
/// stored (the C# casts any byte into its enums); the typed getters convert.
#[derive(Debug, Clone, PartialEq)]
pub struct FormulaManager {
    /// `_unitFormulas`: owner -> kind -> formula.
    unit_formulas: HashMap<u8, HashMap<u8, UnitFormula>>,
    /// `_wearableFormulas`.
    wearable_formulas: HashMap<u8, WearableFormula>,
    /// `_formulas`.
    formulas: HashMap<u32, Formula>,
    /// `_unitVariables`: formula id -> variable type -> key -> variable.
    unit_variables: HashMap<u32, HashMap<u8, HashMap<u32, UnitFormulaVariable>>>,
}

impl Default for FormulaManager {
    fn default() -> Self {
        Self::empty()
    }
}

impl FormulaManager {
    /// A manager with no formulas (unit tests); like the C# after `Load` on empty tables, every
    /// `FormulaOwnerType` has an (empty) unit-formula table.
    pub fn empty() -> FormulaManager {
        FormulaManager {
            unit_formulas: FormulaOwnerType::ALL
                .iter()
                .map(|owner| (owner.raw(), HashMap::new()))
                .collect(),
            wearable_formulas: HashMap::new(),
            formulas: HashMap::new(),
            unit_variables: HashMap::new(),
        }
    }

    /// `FormulaManager.Load()`: reads `unit_formulas`, `unit_formula_variables`,
    /// `wearable_formulas` and `formulas` (`SELECT *`). A formula that fails to compile is logged
    /// and skipped; duplicate keys and unknown owners abort like the C# exceptions.
    pub fn load(conn: &Connection) -> Result<FormulaManager, FormulaLoadError> {
        let mut manager = FormulaManager::empty();
        tracing::info!("Loading formulas...");

        {
            const TABLE: &str = "unit_formulas";
            let mut stmt = conn.prepare("SELECT * from unit_formulas")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let mut formula = UnitFormula {
                    formula: Formula::unprepared(
                        read_u32(row, TABLE, "id")?,
                        read_string(row, TABLE, "formula")?,
                    ),
                    kind_id: read_u8(row, TABLE, "kind_id")?,
                    owner_type_id: read_u8(row, TABLE, "owner_type_id")?,
                };
                if !formula.formula.prepare() {
                    continue;
                }
                let id = formula.id;
                let (owner, kind) = (formula.owner_type_id, formula.kind_id);
                let by_kind = manager
                    .unit_formulas
                    .get_mut(&owner)
                    .ok_or(FormulaLoadError::UnknownOwner { id, owner })?;
                match by_kind.entry(kind) {
                    Entry::Occupied(_) => {
                        return Err(FormulaLoadError::DuplicateUnitFormula { id, owner, kind })
                    }
                    Entry::Vacant(slot) => {
                        slot.insert(formula);
                    }
                }
            }
        }

        {
            const TABLE: &str = "unit_formula_variables";
            let mut stmt = conn.prepare("SELECT * from unit_formula_variables")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let variable = UnitFormulaVariable {
                    formula_id: read_u32(row, TABLE, "unit_formula_id")?,
                    variable_kind_id: read_u8(row, TABLE, "variable_kind_id")?,
                    key: read_u32(row, TABLE, "key")?,
                    value: read_f32(row, TABLE, "value")?,
                };
                let by_key = manager
                    .unit_variables
                    .entry(variable.formula_id)
                    .or_default()
                    .entry(variable.variable_kind_id)
                    .or_default();
                match by_key.entry(variable.key) {
                    Entry::Occupied(_) => {
                        return Err(FormulaLoadError::DuplicateUnitVariable {
                            formula_id: variable.formula_id,
                            kind: variable.variable_kind_id,
                            key: variable.key,
                        })
                    }
                    Entry::Vacant(slot) => {
                        slot.insert(variable);
                    }
                }
            }
        }

        {
            const TABLE: &str = "wearable_formulas";
            let mut stmt = conn.prepare("SELECT * from wearable_formulas")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let id = read_u32(row, TABLE, "id")?;
                let kind = read_u8(row, TABLE, "kind_id")?;
                let mut formula = WearableFormula {
                    formula: Formula::unprepared(id, read_string(row, TABLE, "formula")?),
                    kind_id: kind,
                };
                if !formula.formula.prepare() {
                    continue;
                }
                match manager.wearable_formulas.entry(kind) {
                    Entry::Occupied(_) => {
                        return Err(FormulaLoadError::DuplicateWearableFormula { id, kind })
                    }
                    Entry::Vacant(slot) => {
                        slot.insert(formula);
                    }
                }
            }
        }

        {
            const TABLE: &str = "formulas";
            let mut stmt = conn.prepare("SELECT * from formulas")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let mut formula = Formula::unprepared(
                    read_u32(row, TABLE, "id")?,
                    read_string(row, TABLE, "formula")?,
                );
                if !formula.prepare() {
                    continue;
                }
                match manager.formulas.entry(formula.id) {
                    Entry::Occupied(_) => {
                        return Err(FormulaLoadError::DuplicateFormula(formula.id))
                    }
                    Entry::Vacant(slot) => {
                        slot.insert(formula);
                    }
                }
            }
        }

        tracing::info!("Formulas loaded");
        Ok(manager)
    }

    /// `GetUnitFormula(owner, kind)`.
    pub fn get_unit_formula(
        &self,
        owner: FormulaOwnerType,
        kind: UnitFormulaKind,
    ) -> Option<&UnitFormula> {
        self.get_unit_formula_raw(owner.raw(), kind.raw())
    }

    /// `GetUnitFormula` with raw enum values.
    pub fn get_unit_formula_raw(&self, owner: u8, kind: u8) -> Option<&UnitFormula> {
        self.unit_formulas.get(&owner)?.get(&kind)
    }

    /// `GetUnitVariable(formulaId, type, key)`: the variable's value, `0` when missing.
    pub fn get_unit_variable(
        &self,
        formula_id: u32,
        kind: UnitFormulaVariableType,
        key: u32,
    ) -> f32 {
        self.get_unit_variable_raw(formula_id, kind.raw(), key)
    }

    /// `GetUnitVariable` with a raw variable type.
    pub fn get_unit_variable_raw(&self, formula_id: u32, kind: u8, key: u32) -> f32 {
        self.unit_variables
            .get(&formula_id)
            .and_then(|by_kind| by_kind.get(&kind))
            .and_then(|by_key| by_key.get(&key))
            .map_or(0.0, |variable| variable.value)
    }

    /// `GetWearableFormula(type)`.
    pub fn get_wearable_formula(&self, kind: WearableFormulaType) -> Option<&WearableFormula> {
        self.get_wearable_formula_raw(kind.raw())
    }

    /// `GetWearableFormula` with a raw type.
    pub fn get_wearable_formula_raw(&self, kind: u8) -> Option<&WearableFormula> {
        self.wearable_formulas.get(&kind)
    }

    /// `GetFormula(id)`.
    pub fn get_formula(&self, id: u32) -> Option<&Formula> {
        self.formulas.get(&id)
    }

    /// `GetFormula((uint)kind)`.
    pub fn get_formula_by_kind(&self, kind: FormulaKind) -> Option<&Formula> {
        self.get_formula(kind.raw())
    }

    /// Test-only: adds a `formulas` row the way [`FormulaManager::load`] reads it, so tests can
    /// build data-shaped formula fixtures.
    #[cfg(test)]
    pub fn insert_formula(&mut self, id: u32, text_formula: &str) {
        let mut formula = Formula::unprepared(id, text_formula);
        assert!(formula.prepare(), "test formula {id} must compile");
        self.formulas.insert(id, formula);
    }
}

/// `SqliteDataReader.GetInt64` (`sqlite3_column_int64` conversions).
fn read_i64(
    row: &Row<'_>,
    table: &'static str,
    column: &'static str,
) -> Result<i64, FormulaLoadError> {
    match row.get_ref(column)? {
        ValueRef::Integer(i) => Ok(i),
        ValueRef::Real(r) => Ok(r as i64),
        ValueRef::Text(t) => {
            let text = String::from_utf8_lossy(t);
            let text = text.trim();
            Ok(text
                .parse::<i64>()
                .ok()
                .or_else(|| text.parse::<f64>().ok().map(|r| r as i64))
                .unwrap_or(0))
        }
        ValueRef::Null => Err(FormulaLoadError::NullValue { table, column }),
        ValueRef::Blob(_) => Err(FormulaLoadError::InvalidType { table, column }),
    }
}

/// `SQLiteWrapperReader.GetUInt32` = `(uint)GetInt32(column)` (checked `int` conversion first).
fn read_u32(
    row: &Row<'_>,
    table: &'static str,
    column: &'static str,
) -> Result<u32, FormulaLoadError> {
    let value = read_i64(row, table, column)?;
    let value = i32::try_from(value).map_err(|_| FormulaLoadError::Overflow {
        table,
        column,
        value,
        target: "Int32",
    })?;
    Ok(value as u32)
}

/// `SQLiteWrapperReader.GetByte` (checked `byte` conversion).
fn read_u8(
    row: &Row<'_>,
    table: &'static str,
    column: &'static str,
) -> Result<u8, FormulaLoadError> {
    let value = read_i64(row, table, column)?;
    u8::try_from(value).map_err(|_| FormulaLoadError::Overflow {
        table,
        column,
        value,
        target: "Byte",
    })
}

/// `SQLiteWrapperReader.GetFloat` = `(float)GetDouble`.
fn read_f32(
    row: &Row<'_>,
    table: &'static str,
    column: &'static str,
) -> Result<f32, FormulaLoadError> {
    match row.get_ref(column)? {
        ValueRef::Real(r) => Ok(r as f32),
        ValueRef::Integer(i) => Ok(i as f64 as f32),
        ValueRef::Text(t) => Ok(String::from_utf8_lossy(t)
            .trim()
            .parse::<f64>()
            .unwrap_or(0.0) as f32),
        ValueRef::Null => Err(FormulaLoadError::NullValue { table, column }),
        ValueRef::Blob(_) => Err(FormulaLoadError::InvalidType { table, column }),
    }
}

/// `SQLiteWrapperReader.GetString`.
fn read_string(
    row: &Row<'_>,
    table: &'static str,
    column: &'static str,
) -> Result<String, FormulaLoadError> {
    match row.get_ref(column)? {
        ValueRef::Text(t) | ValueRef::Blob(t) => Ok(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Integer(i) => Ok(i.to_string()),
        ValueRef::Real(r) => Ok(r.to_string()),
        ValueRef::Null => Err(FormulaLoadError::NullValue { table, column }),
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests;
