use std::borrow::Borrow;
use std::cell::Cell;
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, Hasher};

// ---------------------------------------------------------------------------------------------
// Expression engine (Jace 1.0.0)
// ---------------------------------------------------------------------------------------------

/// A Jace `ParseException` (or one of the raw exceptions Jace's parser lets escape) raised by
/// `CalculationEngine.Build`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ParseError {
    #[error("Invalid token \"{token}\" detected at position {position}.")]
    InvalidToken { token: char, position: usize },
    /// Jace indexes past the end of the text for a trailing exponent marker (`"2e"`).
    #[error("Incomplete exponent in the number at position {position}.")]
    IncompleteExponent { position: usize },
    /// A binary `-` with no preceding token (only reachable after a dropped invalid number).
    #[error("Unexpected \"-\" at position {position}.")]
    UnexpectedMinus { position: usize },
    #[error(
        "There is a syntax issue for the operation \"{operation}\" at position {position}. \
         The number of arguments does not match with what is expected."
    )]
    OperationArguments { operation: char, position: usize },
    #[error(
        "There is a syntax issue for the function \"{function}\" at position {position}. \
         The number of arguments does not match with what is expected."
    )]
    FunctionArguments { function: String, position: usize },
    #[error("No matching left bracket found for the right bracket at position {position}.")]
    NoMatchingLeftBracket { position: usize },
    #[error("No matching right bracket found for the left bracket at position {position}.")]
    NoMatchingRightBracket { position: usize },
    /// An argument separator outside of any function call (Jace: "Stack empty.").
    #[error("Unexpected argument separator at position {position}.")]
    UnexpectedArgumentSeparator { position: usize },
    #[error("Unexpected integer constant \"{0}\" found.")]
    UnexpectedIntegerConstant(f64),
    #[error("Unexpected floating point constant \"{0}\" found.")]
    UnexpectedFloatingPointConstant(f64),
    #[error("The syntax of the provided formula is not valid.")]
    InvalidSyntax,
    /// Nothing to evaluate (Jace: "Sequence contains no elements").
    #[error("The formula is empty.")]
    Empty,
}

/// A failure while evaluating a parsed [`Expression`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvalError {
    /// Jace `VariableNotDefinedException`.
    #[error("The variable \"{0}\" used is not defined.")]
    VariableNotDefined(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

impl BinOp {
    fn from_char(c: char) -> Option<Self> {
        Some(match c {
            '+' => Self::Add,
            '-' => Self::Sub,
            '*' => Self::Mul,
            '/' => Self::Div,
            '%' => Self::Mod,
            '^' => Self::Pow,
            '<' => Self::Lt,
            '≤' => Self::Le,
            '>' => Self::Gt,
            '≥' => Self::Ge,
            '=' => Self::Eq,
            '≠' => Self::Ne,
            '&' => Self::And,
            '|' => Self::Or,
            _ => return None,
        })
    }

    fn apply(self, a: f64, b: f64) -> f64 {
        let bool_value = |v: bool| if v { 1.0 } else { 0.0 };
        match self {
            Self::Add => a + b,
            Self::Sub => a - b,
            Self::Mul => a * b,
            Self::Div => a / b,
            Self::Mod => a % b,
            Self::Pow => a.powf(b),
            Self::Lt => bool_value(a < b),
            Self::Le => bool_value(a <= b),
            Self::Gt => bool_value(a > b),
            Self::Ge => bool_value(a >= b),
            Self::Eq => bool_value(a == b),
            Self::Ne => bool_value(a != b),
            Self::And => bool_value(a != 0.0 && b != 0.0),
            Self::Or => bool_value(a != 0.0 || b != 0.0),
        }
    }
}

/// Jace `AstBuilder.operationPrecedence` (`'_'` is Jace's internal unary minus).
fn precedence(op: char) -> u8 {
    match op {
        '&' | '|' => 1,
        '<' | '>' | '≤' | '≥' | '≠' | '=' => 2,
        '+' | '-' => 3,
        '*' | '/' | '%' => 4,
        '^' => 5,
        _ => 6, // '_'
    }
}

/// Jace `AstBuilder.IsLeftAssociativeOperation`.
fn is_left_associative(op: char) -> bool {
    matches!(op, '*' | '+' | '-' | '/')
}

/// The functions known to the engine: Jace's built-ins plus the four registered by
/// `FormulaManager.Load`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Func {
    Sin,
    Cos,
    Csc,
    Sec,
    Asin,
    Acos,
    Tan,
    Cot,
    Atan,
    Acot,
    Loge,
    Log10,
    Logn,
    Sqrt,
    Abs,
    If,
    IfLess,
    IfMore,
    IfEqual,
    Ceiling,
    Floor,
    Truncate,
    Round,
    Random,
    Max,
    Min,
    Avg,
    Median,
    Clamp,
    IfNegative,
    IfPositive,
    IfZero,
}

impl Func {
    const ALL: [(&'static str, Func); 32] = [
        ("sin", Func::Sin),
        ("cos", Func::Cos),
        ("csc", Func::Csc),
        ("sec", Func::Sec),
        ("asin", Func::Asin),
        ("acos", Func::Acos),
        ("tan", Func::Tan),
        ("cot", Func::Cot),
        ("atan", Func::Atan),
        ("acot", Func::Acot),
        ("loge", Func::Loge),
        ("log10", Func::Log10),
        ("logn", Func::Logn),
        ("sqrt", Func::Sqrt),
        ("abs", Func::Abs),
        ("if", Func::If),
        ("ifless", Func::IfLess),
        ("ifmore", Func::IfMore),
        ("ifequal", Func::IfEqual),
        ("ceiling", Func::Ceiling),
        ("floor", Func::Floor),
        ("truncate", Func::Truncate),
        ("round", Func::Round),
        ("random", Func::Random),
        ("max", Func::Max),
        ("min", Func::Min),
        ("avg", Func::Avg),
        ("median", Func::Median),
        ("clamp", Func::Clamp),
        ("if_negative", Func::IfNegative),
        ("if_positive", Func::IfPositive),
        ("if_zero", Func::IfZero),
    ];

    /// Jace's function registry lookup, which ignores case.
    fn lookup(name: &str) -> Option<Func> {
        Self::ALL
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|&(_, f)| f)
    }

    /// The fixed parameter count, or `None` for Jace's dynamic (`params double[]`) functions.
    fn arity(self) -> Option<usize> {
        Some(match self {
            Self::Random => 0,
            Self::Sin
            | Self::Cos
            | Self::Csc
            | Self::Sec
            | Self::Asin
            | Self::Acos
            | Self::Tan
            | Self::Cot
            | Self::Atan
            | Self::Acot
            | Self::Loge
            | Self::Log10
            | Self::Sqrt
            | Self::Abs
            | Self::Ceiling
            | Self::Floor
            | Self::Truncate
            | Self::Round => 1,
            Self::Logn => 2,
            Self::If | Self::Clamp | Self::IfNegative | Self::IfPositive | Self::IfZero => 3,
            Self::IfLess | Self::IfMore | Self::IfEqual => 4,
            Self::Max | Self::Min | Self::Avg | Self::Median => return None,
        })
    }

    fn apply(self, a: &[f64]) -> f64 {
        match self {
            Self::Sin => a[0].sin(),
            Self::Cos => a[0].cos(),
            Self::Csc => 1.0 / a[0].sin(),
            Self::Sec => 1.0 / a[0].cos(),
            Self::Asin => a[0].asin(),
            Self::Acos => a[0].acos(),
            Self::Tan => a[0].tan(),
            Self::Cot => 1.0 / a[0].tan(),
            Self::Atan => a[0].atan(),
            Self::Acot => (1.0 / a[0]).atan(),
            Self::Loge => a[0].ln(),
            Self::Log10 => a[0].log10(),
            Self::Logn => dotnet_log(a[0], a[1]),
            Self::Sqrt => a[0].sqrt(),
            Self::Abs => a[0].abs(),
            Self::If => {
                if a[0] != 0.0 {
                    a[1]
                } else {
                    a[2]
                }
            }
            Self::IfLess => {
                if a[0] < a[1] {
                    a[2]
                } else {
                    a[3]
                }
            }
            Self::IfMore => {
                if a[0] > a[1] {
                    a[2]
                } else {
                    a[3]
                }
            }
            Self::IfEqual => {
                if a[0] == a[1] {
                    a[2]
                } else {
                    a[3]
                }
            }
            Self::Ceiling => a[0].ceil(),
            Self::Floor => a[0].floor(),
            Self::Truncate => a[0].trunc(),
            Self::Round => a[0].round_ties_even(),
            Self::Random => next_random(),
            Self::Max => linq_max(a),
            Self::Min => linq_min(a),
            // `Enumerable.Average`: a sequential sum starting at +0.0.
            Self::Avg => a.iter().fold(0.0, |sum, x| sum + x) / a.len() as f64,
            Self::Median => median(a),
            // FormulaManager.Load: (a, b, c) => a < b ? b : a > c ? c : a
            Self::Clamp => {
                if a[0] < a[1] {
                    a[1]
                } else if a[0] > a[2] {
                    a[2]
                } else {
                    a[0]
                }
            }
            // (a, b, c) => a < 0 ? b : c
            Self::IfNegative => {
                if a[0] < 0.0 {
                    a[1]
                } else {
                    a[2]
                }
            }
            // (a, b, c) => a > 0 ? b : c
            Self::IfPositive => {
                if a[0] > 0.0 {
                    a[1]
                } else {
                    a[2]
                }
            }
            // (a, b, c) => a == 0 ? b : c
            Self::IfZero => {
                if a[0] == 0.0 {
                    a[1]
                } else {
                    a[2]
                }
            }
        }
    }
}

/// `System.Math.Log(a, newBase)`.
fn dotnet_log(a: f64, new_base: f64) -> f64 {
    if a.is_nan() {
        return a;
    }
    if new_base.is_nan() {
        return new_base;
    }
    if new_base == 1.0 {
        return f64::NAN;
    }
    if a != 1.0 && (new_base == 0.0 || new_base == f64::INFINITY) {
        return f64::NAN;
    }
    a.ln() / new_base.ln()
}

/// `Enumerable.Max(IEnumerable<double>)`: NaN only wins when every value is NaN.
fn linq_max(values: &[f64]) -> f64 {
    let mut iter = values.iter().copied();
    let Some(mut value) = iter.next() else {
        return f64::NAN;
    };
    while value.is_nan() {
        match iter.next() {
            Some(next) => value = next,
            None => return value,
        }
    }
    for x in iter {
        if x > value {
            value = x;
        }
    }
    value
}

/// `Enumerable.Min(IEnumerable<double>)`: any NaN wins.
fn linq_min(values: &[f64]) -> f64 {
    let mut iter = values.iter().copied();
    let Some(mut value) = iter.next() else {
        return f64::NAN;
    };
    if value.is_nan() {
        return value;
    }
    for x in iter {
        if x < value {
            value = x;
        } else if x.is_nan() {
            return x;
        }
    }
    value
}

/// .NET `Double.CompareTo`: NaN sorts first, `-0.0 == 0.0`.
fn dotnet_compare(a: f64, b: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if a < b {
        Ordering::Less
    } else if a > b {
        Ordering::Greater
    } else if a == b {
        Ordering::Equal
    } else if a.is_nan() {
        if b.is_nan() {
            Ordering::Equal
        } else {
            Ordering::Less
        }
    } else {
        Ordering::Greater
    }
}

/// Jace `MathExtended.Median`: `NthOrderStatistic(list, (count - 1) / 2)`, a quickselect with a
/// Lomuto partition around the last element (no random pivot). Reproduced exactly because it
/// decides which of several equal-comparing values (`-0.0` / `0.0`) is returned.
fn median(values: &[f64]) -> f64 {
    let mut list = values.to_vec();
    if list.is_empty() {
        return f64::NAN;
    }
    let n = (list.len() as isize - 1) / 2;
    let (mut start, mut end) = (0isize, list.len() as isize - 1);
    loop {
        // MathExtended.Partition(list, start, end, rnd: null)
        let pivot = list[end as usize];
        let mut last_low = start - 1;
        for i in start..end {
            if dotnet_compare(list[i as usize], pivot) != std::cmp::Ordering::Greater {
                last_low += 1;
                list.swap(i as usize, last_low as usize);
            }
        }
        last_low += 1;
        list.swap(end as usize, last_low as usize);

        let pivot_index = last_low;
        if pivot_index == n {
            return list[pivot_index as usize];
        }
        if n < pivot_index {
            end = pivot_index - 1;
        } else {
            start = pivot_index + 1;
        }
    }
}

thread_local! {
    static RANDOM_STATE: Cell<u64> = Cell::new({
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(0x9E37_79B9_7F4A_7C15);
        hasher.finish()
    });
}

/// `random()`: uniform in `[0, 1)` (SplitMix64; Jace uses `System.Random.NextDouble`).
fn next_random() -> f64 {
    RANDOM_STATE.with(|state| {
        let s = state.get().wrapping_add(0x9E37_79B9_7F4A_7C15);
        state.set(s);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    })
}

/// Jace's constant registry (`e`, `pi`), consulted case-insensitively when a variable is not in
/// the parameter map.
fn constant(name: &str) -> Option<f64> {
    if name.eq_ignore_ascii_case("e") {
        Some(std::f64::consts::E)
    } else if name.eq_ignore_ascii_case("pi") {
        Some(std::f64::consts::PI)
    } else {
        None
    }
}

/// Jace AST node (`Operation` subclasses).
#[derive(Debug, Clone, PartialEq)]
enum Node {
    /// `IntegerConstant` (`integer == true`) or `FloatingPointConstant`; the distinction matters
    /// to the optimizer's `x * 0.0` rule.
    Const {
        value: f64,
        integer: bool,
    },
    Var(Box<str>),
    Neg(Box<Node>),
    Binary(BinOp, Box<Node>, Box<Node>),
    Call(Func, Vec<Node>),
}

impl Node {
    fn float(value: f64) -> Node {
        Node::Const {
            value,
            integer: false,
        }
    }

    /// `Operation.DependsOnVariables`.
    fn depends_on_variables(&self) -> bool {
        match self {
            Node::Const { .. } => false,
            Node::Var(_) => true,
            Node::Neg(a) => a.depends_on_variables(),
            Node::Binary(_, a, b) => a.depends_on_variables() || b.depends_on_variables(),
            Node::Call(_, args) => args.iter().any(Node::depends_on_variables),
        }
    }

    /// `Operation.IsIdempotent` (only `random` is not).
    fn is_idempotent(&self) -> bool {
        match self {
            Node::Const { .. } | Node::Var(_) => true,
            Node::Neg(a) => a.is_idempotent(),
            Node::Binary(_, a, b) => a.is_idempotent() && b.is_idempotent(),
            Node::Call(f, args) => *f != Func::Random && args.iter().all(Node::is_idempotent),
        }
    }

    fn is_float_zero(&self) -> bool {
        matches!(self, Node::Const { value, integer: false } if *value == 0.0)
    }

    fn eval(&self, lookup: &dyn Fn(&str) -> Option<f64>) -> Result<f64, EvalError> {
        Ok(match self {
            Node::Const { value, .. } => *value,
            Node::Var(name) => lookup(name)
                .or_else(|| constant(name))
                .ok_or_else(|| EvalError::VariableNotDefined(name.to_string()))?,
            Node::Neg(a) => -a.eval(lookup)?,
            Node::Binary(op, a, b) => {
                let a = a.eval(lookup)?;
                let b = b.eval(lookup)?;
                op.apply(a, b)
            }
            Node::Call(f, args) => {
                // Every argument is evaluated, like the C# delegate invocation.
                let mut values = [0.0f64; 4];
                if args.len() <= values.len() {
                    for (slot, arg) in values.iter_mut().zip(args) {
                        *slot = arg.eval(lookup)?;
                    }
                    f.apply(&values[..args.len()])
                } else {
                    let values = args
                        .iter()
                        .map(|arg| arg.eval(lookup))
                        .collect::<Result<Vec<_>, _>>()?;
                    f.apply(&values)
                }
            }
        })
    }
}

/// Jace `Optimizer.Optimize`.
fn optimize(node: Node) -> Node {
    if !matches!(node, Node::Const { .. }) && !node.depends_on_variables() && node.is_idempotent() {
        let value = node
            .eval(&|_| None)
            .expect("variable-free expressions always evaluate");
        return Node::float(value);
    }
    match node {
        Node::Binary(BinOp::Mul, a, b) => {
            let (a, b) = (optimize(*a), optimize(*b));
            if a.is_float_zero() || b.is_float_zero() {
                Node::float(0.0)
            } else {
                Node::Binary(BinOp::Mul, Box::new(a), Box::new(b))
            }
        }
        Node::Binary(op @ (BinOp::Add | BinOp::Sub | BinOp::Div | BinOp::Pow), a, b) => {
            Node::Binary(op, Box::new(optimize(*a)), Box::new(optimize(*b)))
        }
        Node::Call(f, args) => Node::Call(f, args.into_iter().map(optimize).collect()),
        other => other,
    }
}

#[derive(Debug, Clone, PartialEq)]
enum TokenKind {
    Integer(f64),
    FloatingPoint(f64),
    Text(String),
    /// Binary operator, or `'_'` for unary minus.
    Operation(char),
    LeftBracket,
    RightBracket,
    ArgumentSeparator,
}

#[derive(Debug, Clone, PartialEq)]
struct Token {
    kind: TokenKind,
    position: usize,
}

/// Jace `TokenReader.IsPartOfNumeric`.
fn is_part_of_numeric(c: char, first: bool, formula_sub_part: bool) -> bool {
    c == '.'
        || c.is_ascii_digit()
        || (formula_sub_part && first && c == '-')
        || (!first && (c == 'e' || c == 'E'))
}

/// Jace `TokenReader.IsPartOfVariable`.
fn is_part_of_variable(c: char, first: bool) -> bool {
    c.is_ascii_alphabetic() || (!first && (c.is_ascii_digit() || c == '_'))
}

/// Jace `TokenReader.Read` (`argumentSeparator = ','`, invariant culture).
fn tokenize(text: &str) -> Result<Vec<Token>, ParseError> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut tokens: Vec<Token> = Vec::new();
    let mut formula_sub_part = true;
    // A field of the reader in Jace: only reset by a successfully parsed floating point number.
    let mut is_scientific = false;
    let mut i = 0;
    while i < n {
        if is_part_of_numeric(chars[i], true, formula_sub_part) {
            let start = i;
            let mut buffer = String::new();
            buffer.push(chars[i]);
            i += 1;
            while i < n && is_part_of_numeric(chars[i], false, formula_sub_part) {
                let c = chars[i];
                let scientific = c == 'e' || c == 'E';
                if is_scientific && scientific {
                    return Err(ParseError::InvalidToken {
                        token: c,
                        position: i,
                    });
                }
                if scientific {
                    is_scientific = true;
                    match chars.get(i + 1) {
                        None => return Err(ParseError::IncompleteExponent { position: start }),
                        Some('-') => {
                            buffer.push(c);
                            i += 1;
                        }
                        Some(_) => {}
                    }
                }
                buffer.push(chars[i]);
                i += 1;
            }

            if let Ok(value) = buffer.parse::<i32>() {
                tokens.push(Token {
                    kind: TokenKind::Integer(f64::from(value)),
                    position: start,
                });
                formula_sub_part = false;
            } else if let Ok(value) = buffer.parse::<f64>() {
                tokens.push(Token {
                    kind: TokenKind::FloatingPoint(value),
                    position: start,
                });
                is_scientific = false;
                formula_sub_part = false;
            } else if buffer == "-" {
                tokens.push(Token {
                    kind: TokenKind::Operation('_'),
                    position: start,
                });
            }
            // Anything else is silently skipped, exactly like Jace.

            if i == n {
                break;
            }
        }

        // Jace falls through: the character that ended a number is examined in the same pass.
        if is_part_of_variable(chars[i], true) {
            let start = i;
            let mut buffer = String::new();
            buffer.push(chars[i]);
            i += 1;
            while i < n && is_part_of_variable(chars[i], false) {
                buffer.push(chars[i]);
                i += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Text(buffer),
                position: start,
            });
            formula_sub_part = false;
            if i == n {
                break;
            }
        }

        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let push_op = |op: char, tokens: &mut Vec<Token>| {
            tokens.push(Token {
                kind: TokenKind::Operation(op),
                position: i,
            });
        };
        match c {
            ',' => {
                tokens.push(Token {
                    kind: TokenKind::ArgumentSeparator,
                    position: i,
                });
                formula_sub_part = false;
            }
            ' ' => {}
            '+' | '-' | '*' | '/' | '^' | '%' | '≤' | '≥' | '≠' => {
                let op = if c == '-' {
                    let previous = tokens
                        .last()
                        .ok_or(ParseError::UnexpectedMinus { position: i })?;
                    let binary = matches!(
                        previous.kind,
                        TokenKind::Integer(_)
                            | TokenKind::FloatingPoint(_)
                            | TokenKind::Text(_)
                            | TokenKind::RightBracket
                    );
                    if binary {
                        '-'
                    } else {
                        '_'
                    }
                } else {
                    c
                };
                push_op(op, &mut tokens);
                formula_sub_part = true;
            }
            '(' => {
                tokens.push(Token {
                    kind: TokenKind::LeftBracket,
                    position: i,
                });
                formula_sub_part = true;
            }
            ')' => {
                tokens.push(Token {
                    kind: TokenKind::RightBracket,
                    position: i,
                });
                formula_sub_part = false;
            }
            '<' | '>' => {
                if next == Some('=') {
                    push_op(if c == '<' { '≤' } else { '≥' }, &mut tokens);
                    i += 1;
                } else {
                    push_op(c, &mut tokens);
                }
                // Unlike the arithmetic operators, Jace resets the flag here, so a following
                // `-` is an operator token, never part of a negative literal.
                formula_sub_part = false;
            }
            '!' | '=' | '&' | '|' => {
                let (second, op) = match c {
                    '!' => ('=', '≠'),
                    '=' => ('=', '='),
                    _ => (c, c),
                };
                if next != Some(second) {
                    return Err(ParseError::InvalidToken {
                        token: c,
                        position: i,
                    });
                }
                push_op(op, &mut tokens);
                i += 1;
                formula_sub_part = false;
            }
            _ => {
                return Err(ParseError::InvalidToken {
                    token: c,
                    position: i,
                })
            }
        }
        i += 1;
    }
    Ok(tokens)
}

#[derive(Debug)]
enum StackEntry {
    Operation(char, usize),
    Function(Func, String, usize),
    LeftBracket(usize),
}

/// Jace `AstBuilder` (shunting-yard, including its leniencies).
struct AstBuilder {
    result: Vec<Node>,
    operators: Vec<StackEntry>,
    parameter_count: Vec<usize>,
}

impl AstBuilder {
    fn build(tokens: Vec<Token>) -> Result<Node, ParseError> {
        let mut builder = AstBuilder {
            result: Vec::new(),
            operators: Vec::new(),
            parameter_count: Vec::new(),
        };
        for token in tokens {
            let position = token.position;
            match token.kind {
                TokenKind::Integer(value) => builder.result.push(Node::Const {
                    value,
                    integer: true,
                }),
                TokenKind::FloatingPoint(value) => builder.result.push(Node::float(value)),
                TokenKind::Text(name) => match Func::lookup(&name) {
                    Some(f) => {
                        builder
                            .operators
                            .push(StackEntry::Function(f, name, position));
                        builder.parameter_count.push(1);
                    }
                    None => builder.result.push(Node::Var(name.into_boxed_str())),
                },
                TokenKind::LeftBracket => builder.operators.push(StackEntry::LeftBracket(position)),
                TokenKind::RightBracket => builder.pop_operations(Some(&token.kind), position)?,
                TokenKind::ArgumentSeparator => {
                    builder.pop_operations(Some(&token.kind), position)?;
                    let count = builder
                        .parameter_count
                        .pop()
                        .ok_or(ParseError::UnexpectedArgumentSeparator { position })?;
                    builder.parameter_count.push(count + 1);
                }
                TokenKind::Operation(op) => {
                    while let Some(top) = builder.operators.last() {
                        match *top {
                            StackEntry::Operation(top_op, _) => {
                                let pop = (is_left_associative(op)
                                    && precedence(op) <= precedence(top_op))
                                    || precedence(op) < precedence(top_op);
                                if !pop {
                                    break;
                                }
                                builder.pop_one()?;
                            }
                            // A function on top of the stack is always reduced first.
                            StackEntry::Function(..) => builder.pop_one()?,
                            StackEntry::LeftBracket(_) => break,
                        }
                    }
                    builder.operators.push(StackEntry::Operation(op, position));
                }
            }
        }
        builder.pop_operations(None, 0)?;
        builder.verify_result()
    }

    /// Reduces the operator / function on top of the operator stack.
    fn pop_one(&mut self) -> Result<(), ParseError> {
        match self.operators.pop() {
            Some(StackEntry::Operation(op, position)) => {
                let error = ParseError::OperationArguments {
                    operation: op,
                    position,
                };
                let node = if op == '_' {
                    Node::Neg(Box::new(self.result.pop().ok_or(error)?))
                } else {
                    if self.result.len() < 2 {
                        return Err(error);
                    }
                    let b = self.result.pop().expect("checked length");
                    let a = self.result.pop().expect("checked length");
                    let op = BinOp::from_char(op).expect("tokenizer only emits known operators");
                    Node::Binary(op, Box::new(a), Box::new(b))
                };
                self.result.push(node);
            }
            Some(StackEntry::Function(f, name, position)) => {
                let count = self.parameter_count.pop().unwrap_or(0);
                // Fixed-arity functions ignore the counted arguments (`sin(1,2)` reduces as
                // `1 sin(2)` and then fails on the leftover constant).
                let wanted = f.arity().unwrap_or(count);
                if self.result.len() < wanted {
                    return Err(ParseError::FunctionArguments {
                        function: name,
                        position,
                    });
                }
                let args = self.result.split_off(self.result.len() - wanted);
                self.result.push(Node::Call(f, args));
            }
            Some(StackEntry::LeftBracket(_)) | None => {
                unreachable!("pop_one is only called for operations and functions")
            }
        }
        Ok(())
    }

    /// Jace `AstBuilder.PopOperations`: `current` is the right bracket / argument separator being
    /// processed, `None` at the end of the formula.
    fn pop_operations(
        &mut self,
        current: Option<&TokenKind>,
        position: usize,
    ) -> Result<(), ParseError> {
        while matches!(
            self.operators.last(),
            Some(StackEntry::Operation(..) | StackEntry::Function(..))
        ) {
            self.pop_one()?;
        }
        let until_left_bracket = current == Some(&TokenKind::RightBracket);
        match self.operators.last() {
            Some(StackEntry::LeftBracket(_)) if until_left_bracket => {
                self.operators.pop();
            }
            _ if until_left_bracket => {
                return Err(ParseError::NoMatchingLeftBracket { position });
            }
            Some(StackEntry::LeftBracket(left)) if current.is_none() => {
                return Err(ParseError::NoMatchingRightBracket { position: *left });
            }
            _ => {}
        }
        Ok(())
    }

    /// Jace `AstBuilder.VerifyResultStack`.
    fn verify_result(mut self) -> Result<Node, ParseError> {
        if self.result.len() > 1 {
            // Stack.ToArray() is top-first; Jace skips the top element.
            for node in self.result.iter().rev().skip(1) {
                if let Node::Const { value, integer } = *node {
                    return Err(if integer {
                        ParseError::UnexpectedIntegerConstant(value)
                    } else {
                        ParseError::UnexpectedFloatingPointConstant(value)
                    });
                }
            }
            return Err(ParseError::InvalidSyntax);
        }
        self.result.pop().ok_or(ParseError::Empty)
    }
}

/// A formula compiled once by [`Expression::parse`] (Jace `CalculationEngine.Build`) and
/// evaluated many times with [`Expression::evaluate`].
#[derive(Debug, Clone, PartialEq)]
pub struct Expression {
    root: Node,
}

impl Expression {
    /// `CalculationEngine.Build(formulaText)` with the `FormulaManager.Load` options
    /// (optimizer on, case-sensitive variables, invariant culture).
    pub fn parse(text: &str) -> Result<Expression, ParseError> {
        let tokens = tokenize(text)?;
        let root = optimize(AstBuilder::build(tokens)?);
        Ok(Expression { root })
    }

    /// Runs the compiled formula. Fails only when a variable is neither in `params` (looked up
    /// case-sensitively) nor one of the constants `e` / `pi`.
    pub fn evaluate<K, S>(&self, params: &HashMap<K, f64, S>) -> Result<f64, EvalError>
    where
        K: Borrow<str> + Eq + Hash,
        S: BuildHasher,
    {
        self.root.eval(&|name| params.get(name).copied())
    }
}
