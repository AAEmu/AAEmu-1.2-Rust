use super::*;

fn eval_with(text: &str, params: &[(&str, f64)]) -> Result<f64, EvalError> {
    let params: HashMap<&str, f64> = params.iter().copied().collect();
    Expression::parse(text)
        .unwrap_or_else(|e| panic!("{text:?} failed to parse: {e}"))
        .evaluate(&params)
}

fn eval(text: &str) -> f64 {
    eval_with(text, &[]).unwrap()
}

fn parses(text: &str) -> bool {
    Expression::parse(text).is_ok()
}

#[test]
fn precedence_and_associativity() {
    assert_eq!(eval("1+2*3"), 7.0);
    assert_eq!(eval("10-4-3"), 3.0);
    assert_eq!(eval("100/10/5"), 2.0);
    assert_eq!(eval("2/2*3"), 3.0);
    assert_eq!(eval("2^3^2"), 512.0); // right-associative
    assert_eq!(eval("2^3^2^0"), 8.0);
    assert_eq!(eval("2^2*3"), 12.0);
    assert_eq!(eval("2 * (3 + 4) ^ 2"), 98.0);
    // `%` is right-associative and the incoming operator decides ties.
    assert_eq!(eval("2*3%4"), 6.0);
    assert_eq!(eval("8%3*2"), 4.0);
    assert_eq!(eval("10 / 2 % 3"), 5.0);
    assert_eq!(eval("10 % 4 / 2"), 1.0);
    assert_eq!(eval("2%5%3"), 0.0);
    assert_eq!(eval("2 ^ 3 % 5"), 3.0);
    assert_eq!(eval("2 % 3 ^ 2"), 2.0);
    assert_eq!(eval("10 - 2 % 3"), 8.0);
    assert_eq!(
        eval("1 + 2 + 3 + 4 * 5 ^ 2 ^ 0.5 - 6 / 3 % 4"),
        42.954070969341686
    );
}

#[test]
fn unary_minus() {
    assert_eq!(eval("-2^2"), 4.0); // unary minus binds tighter than ^
    assert_eq!(eval("0-2^2"), -4.0);
    assert_eq!(eval_with("-x^2", &[("x", 2.0)]), Ok(4.0));
    assert_eq!(eval("2^-2"), 0.25);
    assert_eq!(eval("2^-3^2"), 512.0);
    assert_eq!(eval("2*-3"), -6.0);
    assert_eq!(eval("2--3"), 5.0);
    assert_eq!(eval("--2"), 2.0);
    assert_eq!(eval("- -2"), 2.0);
    assert_eq!(eval("2+-3*4"), -10.0);
    assert_eq!(eval("-(2+3)"), -5.0);
    assert_eq!(eval("-sin(1)"), -(1f64.sin()));
    assert_eq!(eval_with("3*-x^2", &[("x", 2.0)]), Ok(12.0));
    assert_eq!(eval("-2-2"), -4.0);
    assert!(eval("(-2)^0.5").is_nan());
    // `-0` is an integer literal (+0), `- 0` a unary minus (-0).
    assert!(eval("-0").is_sign_positive());
    assert!(eval("- 0").is_sign_negative());
    assert!(eval("--0").is_sign_positive());
    // No unary plus.
    assert!(!parses("+3"));
    assert!(!parses("2-+3"));
}

#[test]
fn numbers() {
    assert_eq!(eval("2.5e3"), 2500.0);
    assert_eq!(eval("1e-2"), 0.01);
    assert_eq!(eval("2E-1"), 0.2);
    assert_eq!(eval(".5"), 0.5);
    assert_eq!(eval("5."), 5.0);
    assert_eq!(eval("-.5"), -0.5);
    assert_eq!(eval("99999999999"), 99999999999.0);
    assert_eq!(eval("1e400"), f64::INFINITY);
    assert!(!parses("1.5E+2"));
    assert!(!parses("1,5"));
    assert!(!parses("2e"));
    assert!(!parses("1e2e3"));
    // Jace silently drops malformed numbers.
    assert_eq!(eval_with("1.2.3 x", &[("x", 7.0)]), Ok(7.0));
    assert!(!parses("1.2.3"));
}

#[test]
fn division_and_ieee() {
    assert_eq!(eval("1/0"), f64::INFINITY);
    assert_eq!(eval("-1/0"), f64::NEG_INFINITY);
    assert!(eval("0/0").is_nan());
    assert!(eval("10 % 0").is_nan());
    assert_eq!(eval("-7%3"), -1.0);
    assert_eq!(eval("7.5%2"), 1.5);
    assert_eq!(eval("0.1+0.2"), 0.30000000000000004);
}

#[test]
fn comparisons_and_logic() {
    assert_eq!(eval("1<2"), 1.0);
    assert_eq!(eval("2<1"), 0.0);
    assert_eq!(eval("1<=1"), 1.0);
    assert_eq!(eval("1>=2"), 0.0);
    assert_eq!(eval("1==1"), 1.0);
    assert_eq!(eval("1!=1"), 0.0);
    assert_eq!(eval("1≤2"), 1.0);
    assert_eq!(eval("1≥2"), 0.0);
    assert_eq!(eval("1≠2"), 1.0);
    assert_eq!(eval("1<2&&2<3"), 1.0);
    assert_eq!(eval("2&&3"), 1.0);
    assert_eq!(eval("1&&0"), 0.0);
    assert_eq!(eval("0||5"), 1.0);
    assert_eq!(eval("0||0"), 0.0);
    assert_eq!(eval("1+1<3"), 1.0);
    // Right-associative comparisons / logic.
    assert_eq!(eval("1<2==1"), 0.0);
    assert_eq!(eval("3 > 2 > 1"), 1.0);
    assert_eq!(eval("1 < 2 < 3"), 0.0);
    assert_eq!(eval("1 || 0 && 0"), 1.0);
    assert_eq!(eval("0||1&&0"), 0.0);
    assert_eq!(eval_with("x&&1", &[("x", f64::NAN)]), Ok(1.0));
    assert_eq!(eval_with("x!=x", &[("x", f64::NAN)]), Ok(1.0));
    for bad in ["1=1", "1&0", "1|0", "!1", "1<>2", "1 = = 1", "1&&&0"] {
        assert!(!parses(bad), "{bad}");
    }
}

#[test]
fn builtin_functions() {
    let close = |text: &str, expected: f64| {
        let v = eval(text);
        assert!(
            (v - expected).abs() < 1e-12,
            "{text} = {v}, expected {expected}"
        );
    };
    close("sin(0)", 0.0);
    close("cos(0)", 1.0);
    close("csc(1)", 1.1883951057781212);
    close("sec(1)", 1.8508157176809255);
    close("asin(1)", std::f64::consts::FRAC_PI_2);
    close("acos(1)", 0.0);
    close("tan(1)", 1.5574077246549023);
    close("cot(1)", 0.6420926159343306);
    close("atan(1)", std::f64::consts::FRAC_PI_4);
    close("acot(1)", std::f64::consts::FRAC_PI_4);
    close("acot(0)", std::f64::consts::FRAC_PI_2);
    close("loge(e)", 1.0);
    close("log10(100)", 2.0);
    close("logn(8,2)", 3.0);
    close("sqrt(16)", 4.0);
    close("abs(-3)", 3.0);
    assert_eq!(eval("if(1,2,3)"), 2.0);
    assert_eq!(eval("if(0,2,3)"), 3.0);
    assert_eq!(eval("if(1<2,3,4)"), 3.0);
    assert_eq!(eval("ifless(1,2,3,4)"), 3.0);
    assert_eq!(eval("ifmore(1,2,3,4)"), 4.0);
    assert_eq!(eval("ifequal(1,1,3,4)"), 3.0);
    assert_eq!(eval("ceiling(1.2)"), 2.0);
    assert_eq!(eval("floor(-1.2)"), -2.0);
    assert_eq!(eval("truncate(-1.7)"), -1.0);
    assert_eq!(eval("round(2.5)"), 2.0); // banker's rounding
    assert_eq!(eval("round(3.5)"), 4.0);
    assert_eq!(eval("round(-2.5)"), -2.0);
    assert_eq!(eval("max(1,5,3)"), 5.0);
    assert_eq!(eval("min(4,2,8)"), 2.0);
    assert_eq!(eval("max(1)"), 1.0);
    assert_eq!(eval("min(1,2,3,4,5,6,7,8,9,10,0.5)"), 0.5);
    assert_eq!(eval("avg(1,2,3,4)"), 2.5);
    assert_eq!(eval("median(1,2,3,4)"), 2.0);
    assert_eq!(eval("median(9,8,7,6,5)"), 7.0);
    assert_eq!(eval_with("max(x,1)", &[("x", f64::NAN)]), Ok(1.0));
    assert!(eval_with("min(1,x)", &[("x", f64::NAN)]).unwrap().is_nan());
    assert!(eval("logn(1,1)").is_nan());
    let r = eval("random()");
    assert!((0.0..1.0).contains(&r));
    assert_eq!(eval("e"), std::f64::consts::E);
    assert_eq!(eval("pi"), std::f64::consts::PI);
    assert_eq!(eval("PI"), std::f64::consts::PI);
    // Function names are case-insensitive.
    assert_eq!(eval("SIN(0)"), 0.0);
    assert_eq!(eval("MAX(1,2)"), 2.0);
    assert_eq!(eval("CLAMP(5,0,3)"), 3.0);
    // Arity errors and unknown functions.
    for bad in [
        "sin()",
        "sin(1,2)",
        "clamp(1,2)",
        "max()",
        "max(1,)",
        "if(1,2,3,4)",
        "logn(8)",
        "log(100)",
        "pow(2,3)",
        "exp(1)",
        "abs",
        "(1,2)",
    ] {
        assert!(!parses(bad), "{bad}");
    }
}

#[test]
fn minus_after_comparisons_and_separators() {
    // After a comparison / logical operator or `,` a `-` is an operator token, so `-e` is the
    // negated constant rather than a (malformed) number.
    assert_eq!(eval("-1||-e"), 1.0);
    assert_eq!(eval("1<-e"), 0.0);
    assert_eq!(eval("max(1,-e)"), 1.0);
    assert!(!parses("1+-e")); // after `+`, "-e" is lexed as a broken number
    assert!(!parses("-e"));
    assert!(eval("max(-1,-0)").is_sign_negative());
    assert!(eval("max(-1,(-0))").is_sign_positive());
    assert!(eval("ifequal(1,2,10,--0)").is_sign_negative());
    assert!(eval("avg(--0)").is_sign_positive());
}

#[test]
fn median_quickselect() {
    assert_eq!(eval("median(3,1,2)"), 2.0);
    assert_eq!(eval("median(4,1)"), 1.0);
    assert_eq!(eval("median(1,2,3,10)"), 2.0);
    assert_eq!(eval_with("median(x,1,2)", &[("x", f64::NAN)]), Ok(1.0));
    // Which zero wins depends on Jace's partitioning.
    let zeros = [("a", 0.0), ("b", -0.0), ("c", 0.0)];
    assert!(eval_with("median(-1,a,b,c,1)", &zeros)
        .unwrap()
        .is_sign_negative());
    assert!(eval_with("median(-1,b,a,c,1)", &zeros)
        .unwrap()
        .is_sign_positive());
}

#[test]
fn custom_functions() {
    assert_eq!(eval("clamp(5,0,3)"), 3.0);
    assert_eq!(eval("clamp(-1,0,3)"), 0.0);
    assert_eq!(eval("clamp(2,0,3)"), 2.0);
    assert_eq!(eval("if_negative(-1,10,20)"), 10.0);
    assert_eq!(eval("if_negative(0,10,20)"), 20.0);
    assert_eq!(eval("if_positive(1,10,20)"), 10.0);
    assert_eq!(eval("if_positive(0,10,20)"), 20.0);
    assert_eq!(eval("if_zero(0,10,20)"), 10.0);
    assert_eq!(eval("if_zero(1,10,20)"), 20.0);
}

#[test]
fn variables_are_case_sensitive() {
    assert_eq!(eval_with("X", &[("X", 3.0)]), Ok(3.0));
    assert_eq!(
        eval_with("x", &[("X", 3.0)]),
        Err(EvalError::VariableNotDefined("x".into()))
    );
    assert_eq!(eval_with("x+X", &[("x", 1.0), ("X", 10.0)]), Ok(11.0));
    assert_eq!(eval_with("x1_2", &[("x1_2", 7.0)]), Ok(7.0));
    // Parameters override the constants (exact name only).
    assert_eq!(eval_with("e", &[("e", 5.0)]), Ok(5.0));
    assert_eq!(eval_with("PI", &[("pi", 2.0)]), Ok(std::f64::consts::PI));
    assert!(!parses("_x"));
    assert!(!parses("2x"));
    assert!(!parses("x y"));
    assert!(!parses("1\t+\t2"));
}

#[test]
fn optimizer_float_zero_multiplication() {
    // `0.0 * x` is folded to 0 even when x is missing; `0 * x` (integer) is not.
    assert_eq!(eval_with("0.0*x", &[]), Ok(0.0));
    assert_eq!(eval_with("(1-1)*x", &[]), Ok(0.0));
    assert_eq!(eval_with("x*0.0", &[("x", f64::INFINITY)]), Ok(0.0));
    assert!(eval_with("0*x", &[("x", f64::INFINITY)]).unwrap().is_nan());
    assert!(eval_with("0*x", &[]).is_err());
    assert!(eval_with("-(0.0*x)", &[]).is_err());
    assert!(eval_with("(0.0*x)%1", &[]).is_err());
    assert_eq!(eval_with("max(0.0*x,1)", &[]), Ok(1.0));
    assert!(eval("0.0*(-1)").is_sign_negative()); // fully constant: folded, not rewritten
}

#[test]
fn jace_leniencies() {
    assert_eq!(eval("1 2 +"), 3.0);
    assert_eq!(eval("sin 1"), 1f64.sin());
    assert_eq!(eval("sin 1 + 2"), 1f64.sin() + 2.0);
    assert_eq!(eval("2 max()"), 2.0);
    assert_eq!(eval("max((1,2),3)"), 3.0);
    for bad in [
        "", " ", "()", "(1", "1)", "1+", "-", "2**3", "1+()", "abs -2",
    ] {
        assert!(!parses(bad), "{bad:?}");
    }
}

#[test]
fn realistic_formulas() {
    let params = [("level", 50.0), ("sta", 100.0)];
    assert_eq!(
        eval_with("(level * 7 + 50) * (1 + sta / 100)", &params),
        Ok(800.0)
    );
    let f = Formula::new("clamp((level - 10) * 2.5, 0, 100) + if_zero(sta, 1, sta ^ 0.5)");
    let params: HashMap<&str, f64> = params.into_iter().collect();
    assert_eq!(f.evaluate(&params), 100.0 + 10.0);
    let f = Formula::new("floor(level / 10) * 3 + max(1, level - 54)");
    assert_eq!(f.evaluate(&params), 16.0);
}

#[test]
fn formula_evaluate_returns_zero_on_errors() {
    let f = Formula::new("level * 2 + missing");
    assert!(f.is_prepared());
    let params: HashMap<&str, f64> = [("level", 3.0)].into_iter().collect();
    assert_eq!(f.evaluate(&params), 0.0);
    // String keys work too.
    let params: HashMap<String, f64> = [("level".to_string(), 3.0), ("missing".to_string(), 1.0)]
        .into_iter()
        .collect();
    assert_eq!(f.evaluate(&params), 7.0);

    let broken = Formula::new("level * (2");
    assert!(!broken.is_prepared());
    assert_eq!(broken.evaluate(&params), 0.0);
}

#[test]
fn enums_keep_exact_values() {
    assert_eq!(UnitFormulaKind::from_u8(46), Some(UnitFormulaKind::Dodge));
    assert_eq!(UnitFormulaKind::from_u8(3), None);
    assert_eq!(UnitFormulaKind::MeleeParry.raw(), 5);
    assert_eq!(FormulaOwnerType::Shipyard.raw(), 6);
    assert_eq!(FormulaOwnerType::ALL.len(), 7);
    assert_eq!(
        UnitFormulaVariableType::try_from(5),
        Ok(UnitFormulaVariableType::MateKind)
    );
    assert_eq!(WearableFormulaType::from_u8(2), None);
    assert_eq!(
        FormulaKind::from_u32(45),
        Some(FormulaKind::BlessUthstinExtendMaxStat)
    );
    assert_eq!(u32::from(FormulaKind::CastingCancelPercent), 2);
}

fn test_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
            "CREATE TABLE unit_formulas (id INTEGER, formula TEXT, kind_id INTEGER, owner_type_id INTEGER);
             CREATE TABLE unit_formula_variables (id INTEGER, unit_formula_id INTEGER, variable_kind_id INTEGER, key INTEGER, value REAL);
             CREATE TABLE wearable_formulas (id INTEGER, kind_id INTEGER, formula TEXT);
             CREATE TABLE formulas (id INTEGER, formula TEXT);
             INSERT INTO unit_formulas VALUES
                 (1, '(level * 7 + 50) * (1 + sta / 100)', 14, 0),
                 (2, 'level * 2', 22, 0),
                 (3, 'level * (', 23, 0),
                 (4, 'level * 10', 14, 1),
                 (5, 'level + 1', 3, 1);
             INSERT INTO unit_formula_variables VALUES
                 (1, 4, 2, 1000, 1.5),
                 (2, 4, 4, 3, 0.25),
                 (3, 4, 9, 1, 2.0);
             INSERT INTO wearable_formulas VALUES
                 (1, 0, 'slot_factor * 10'),
                 (2, 1, 'slot_factor * 5');
             INSERT INTO formulas VALUES
                 (2, 'clamp(a, 0, 1)'),
                 (22, 'grade * 100'),
                 (99, '1 +');",
        )
        .unwrap();
    conn
}

#[test]
fn load_from_sqlite() {
    let manager = FormulaManager::load(&test_db()).unwrap();

    let max_health = manager
        .get_unit_formula(FormulaOwnerType::Character, UnitFormulaKind::MaxHealth)
        .unwrap();
    assert_eq!(max_health.id, 1);
    assert_eq!(max_health.owner(), Some(FormulaOwnerType::Character));
    assert_eq!(max_health.kind(), Some(UnitFormulaKind::MaxHealth));
    let params: HashMap<&str, f64> = [("level", 50.0), ("sta", 100.0)].into_iter().collect();
    assert_eq!(max_health.evaluate(&params), 800.0);

    // The formula with a syntax error is skipped.
    assert!(manager
        .get_unit_formula(FormulaOwnerType::Character, UnitFormulaKind::Dex)
        .is_none());
    // Kinds the enum does not name are still stored.
    assert_eq!(manager.get_unit_formula_raw(1, 3).unwrap().id, 5);
    assert!(manager.get_unit_formula_raw(1, 3).unwrap().kind().is_none());
    assert!(manager
        .get_unit_formula(FormulaOwnerType::Mate, UnitFormulaKind::MaxHealth)
        .is_none());

    assert_eq!(
        manager.get_unit_variable(4, UnitFormulaVariableType::NpcTemplate, 1000),
        1.5
    );
    assert_eq!(
        manager.get_unit_variable(4, UnitFormulaVariableType::NpcGrade, 3),
        0.25
    );
    assert_eq!(manager.get_unit_variable_raw(4, 9, 1), 2.0);
    assert_eq!(
        manager.get_unit_variable(4, UnitFormulaVariableType::NpcGrade, 4),
        0.0
    );
    assert_eq!(
        manager.get_unit_variable(5, UnitFormulaVariableType::NpcKind, 1),
        0.0
    );

    let armor = manager
        .get_wearable_formula(WearableFormulaType::MaxBaseArmor)
        .unwrap();
    let params: HashMap<&str, f64> = [("slot_factor", 2.0)].into_iter().collect();
    assert_eq!(armor.evaluate(&params), 20.0);
    assert_eq!(armor.kind(), Some(WearableFormulaType::MaxBaseArmor));

    assert_eq!(
        manager
            .get_formula_by_kind(FormulaKind::GradeEnchantCost)
            .unwrap()
            .text_formula,
        "grade * 100"
    );
    assert!(manager.get_formula(2).is_some());
    assert!(manager.get_formula(99).is_none()); // failed to prepare
    assert!(manager.get_formula(3).is_none());
}

#[test]
fn load_rejects_what_the_csharp_throws_on() {
    let conn = test_db();
    conn.execute("INSERT INTO unit_formulas VALUES (6, 'level', 14, 0)", [])
        .unwrap();
    assert!(matches!(
        FormulaManager::load(&conn),
        Err(FormulaLoadError::DuplicateUnitFormula {
            id: 6,
            owner: 0,
            kind: 14
        })
    ));

    let conn = test_db();
    conn.execute("INSERT INTO unit_formulas VALUES (7, 'level', 14, 7)", [])
        .unwrap();
    assert!(matches!(
        FormulaManager::load(&conn),
        Err(FormulaLoadError::UnknownOwner { id: 7, owner: 7 })
    ));
    // ...but only for formulas that compiled.
    let conn = test_db();
    conn.execute("INSERT INTO unit_formulas VALUES (7, '(', 14, 7)", [])
        .unwrap();
    assert!(FormulaManager::load(&conn).is_ok());

    let conn = test_db();
    conn.execute(
        "INSERT INTO unit_formula_variables VALUES (9, 4, 2, 1000, 3.0)",
        [],
    )
    .unwrap();
    assert!(matches!(
        FormulaManager::load(&conn),
        Err(FormulaLoadError::DuplicateUnitVariable {
            formula_id: 4,
            kind: 2,
            key: 1000
        })
    ));

    let conn = test_db();
    conn.execute("INSERT INTO wearable_formulas VALUES (3, 1, 'x')", [])
        .unwrap();
    assert!(matches!(
        FormulaManager::load(&conn),
        Err(FormulaLoadError::DuplicateWearableFormula { id: 3, kind: 1 })
    ));

    let conn = test_db();
    conn.execute("INSERT INTO formulas VALUES (22, 'x')", [])
        .unwrap();
    assert!(matches!(
        FormulaManager::load(&conn),
        Err(FormulaLoadError::DuplicateFormula(22))
    ));

    let conn = test_db();
    conn.execute("INSERT INTO unit_formulas VALUES (8, 'level', 300, 0)", [])
        .unwrap();
    assert!(matches!(
        FormulaManager::load(&conn),
        Err(FormulaLoadError::Overflow {
            column: "kind_id",
            value: 300,
            ..
        })
    ));

    let conn = Connection::open_in_memory().unwrap();
    assert!(matches!(
        FormulaManager::load(&conn),
        Err(FormulaLoadError::Sqlite(_))
    ));
}

/// A broken row evaluated every hit must not log at `ERROR`/`WARN` every time: the first
/// failure is a single warning and later failures fall to `debug` (the caller clamps the
/// `0` result to its neutral value).
#[test]
fn evaluate_logs_a_failure_once_then_only_at_debug() {
    let mut formula = Formula::unprepared(987_654_321, "1 + missing_variable");
    assert!(formula.prepare());
    let params: HashMap<&str, f64> = HashMap::new();
    let (value, events) = crate::test_support::capture_events(|| formula.evaluate(&params));
    assert_eq!(value, 0.0);
    // The first failure is the formula, its parameters and the error (three messages).
    assert_eq!(
        events
            .iter()
            .filter(|(level, _)| *level == tracing::Level::WARN)
            .count(),
        3,
        "{events:?}"
    );
    let (value, events) = crate::test_support::capture_events(|| formula.evaluate(&params));
    assert_eq!(value, 0.0);
    assert!(events
        .iter()
        .all(|(level, _)| *level != tracing::Level::WARN));
    assert!(!events.is_empty());
}

/// Every `Formula::new` formula (item and NPC formulas) has id 0, and the ids of the
/// `formulas`, `unit_formulas` and `wearable_formulas` tables overlap: the warning is once per
/// id and text, so a second broken formula with the same id still warns.
#[test]
fn evaluate_warns_once_per_formula_text_not_per_id() {
    let params: HashMap<&str, f64> = HashMap::new();
    let warnings = |formula: &Formula| {
        let (value, events) = crate::test_support::capture_events(|| formula.evaluate(&params));
        assert_eq!(value, 0.0);
        events
            .iter()
            .filter(|(level, _)| *level == tracing::Level::WARN)
            .count()
    };
    let first = Formula::new("1 + warn_key_first_missing");
    let second = Formula::new("1 + warn_key_second_missing");
    assert_eq!((first.id, second.id), (0, 0));
    assert_eq!(warnings(&first), 3, "the first failure warns");
    assert_eq!(
        warnings(&second),
        3,
        "another formula with the same id warns too"
    );
    assert_eq!(warnings(&first), 0, "later failures only log at debug");
    assert_eq!(warnings(&second), 0, "later failures only log at debug");
}

#[test]
fn empty_manager() {
    let manager = FormulaManager::empty();
    assert!(manager
        .get_unit_formula(FormulaOwnerType::Npc, UnitFormulaKind::Armor)
        .is_none());
    assert_eq!(
        manager.get_unit_variable(1, UnitFormulaVariableType::Unknown, 1),
        0.0
    );
    assert!(manager.get_formula(1).is_none());
    assert!(manager
        .get_wearable_formula(WearableFormulaType::MaxBaseMagicResistance)
        .is_none());
}
