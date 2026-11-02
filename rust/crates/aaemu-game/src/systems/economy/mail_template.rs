//! Quoted arguments of the 1.2 client's system-mail templates (`title('…')`,
//! `body(…,'…')|gift:…;`) and the repair of marketplace mails stored before they were escaped.
//!
//! The client reads these strings in two stages, both of which must be satisfied:
//!
//! 1. **Native pre-parse (stage A), `type == CHARGED` only.** For mail type 9 and client locale
//!    en_us / de / fr (`XlGetLocale()` 2 / 6 / 7) the client runs the CRT `sscanf` on the raw text
//!    (unpacked 1.2 `x2game.dll`, `sub_3948E4C0` for the body and `sub_3948C420` for the list
//!    title):
//!    `sscanf(title, "title(%[^)])")` and `sscanf(text, "body(%[^,],%[^,],%[^)])%s")`. A scanset
//!    stops at the first raw `)` byte (or `,` for the first two body fields), whether or not it
//!    sits inside the quotes, and there is no escape handling: the name is cut at its first `)`,
//!    the rebuilt `title('…')` / `body(…)` loses its tail, the Lua call is a syntax error and the
//!    mailbox shows the raw text. Stage A then drops the outer quotes, splits the name on `|`,
//!    and re-wraps it with `"title('%s')"` / `"body(%s,%s,'%s')%s"` without escaping anything.
//! 2. **Lua `loadstring` (stage B), every mail whose sender starts with `.`.**
//!    `GetMailText` (`locale_helper.lua`) evaluates `return locale.mail<sender>.<title or body
//!    text>` with the Lua 5.1.1 lexer (`cryscriptsystem.dll`, `read_string`): `\'`, `\\` and the
//!    decimal `\ddd` escape (up to three digits) are decoded there.
//!
//! So a quoted argument has to be written as a Lua string literal that carries no raw `(` / `)`:
//! `\` becomes `\\`, `'` becomes `\'`, `(` becomes `\040` and `)` becomes `\041`. Stage A leaves
//! backslash sequences untouched and Lua turns them back into the original characters. `|` in a
//! name would still be split or erased by the client, but no 1.2 item name contains one.
//!
//! The C# server wrote the names unescaped (and only the cash shop escaped `'`).

use super::mail::{mail_type, BaseMail};

/// Escapes `value` for use between the single quotes of a client mail template argument (see the
/// module documentation). The backslash is escaped in the same pass, so nothing is escaped twice.
pub fn quote_arg(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '(' => out.push_str("\\040"),
            ')' => out.push_str("\\041"),
            c => out.push(c),
        }
    }
    out
}

/// Rewrites the raw `(` / `)` of an already Lua-escaped argument to `\040` / `\041`. Backslash
/// pairs are copied as they are (`\\`, `\'`, `\040`), except `\(` and `\)`, which Lua already
/// reads as the bare character and which therefore become the decimal escape as well. Returns
/// `None` when nothing had to change.
fn escape_parentheses(arg: &str) -> Option<String> {
    if !arg.contains(['(', ')']) {
        return None;
    }
    let mut out = String::with_capacity(arg.len() + 8);
    let mut chars = arg.chars();
    while let Some(c) = chars.next() {
        match c {
            '(' => out.push_str("\\040"),
            ')' => out.push_str("\\041"),
            '\\' => match chars.next() {
                Some('(') => out.push_str("\\040"),
                Some(')') => out.push_str("\\041"),
                Some(next) => {
                    out.push('\\');
                    out.push(next);
                }
                None => out.push('\\'),
            },
            c => out.push(c),
        }
    }
    Some(out)
}

/// `title('…')` as [`CommercialMail`](super::mail_kinds::CommercialMail) wrote it before the
/// escaping: the argument between `title('` and the closing `')`.
fn repair_title(title: &str) -> Option<String> {
    let arg = title.strip_prefix("title('")?.strip_suffix("')")?;
    let fixed = escape_parentheses(arg)?;
    Some(format!("title('{fixed}')"))
}

/// `body(<true|false>,'<gifter>','<name>')|gift:<0|1>;|refund:<0|1>;|limit:…;` as
/// [`CommercialMail`](super::mail_kinds::CommercialMail) wrote it: the name is the part between
/// the first `','` (it cannot occur in an escaped name) and the last `')|gift:` (the pipe
/// arguments never contain a quote).
fn repair_body(text: &str) -> Option<String> {
    let rest = text
        .strip_prefix("body(true,'")
        .or_else(|| text.strip_prefix("body(false,'"))?;
    let head_len = text.len() - rest.len();
    let name_start = head_len + rest.find("','")? + 3;
    let name_end = text.rfind("')|gift:")?;
    if name_end < name_start || !text[name_end..].ends_with(';') {
        return None;
    }
    let fixed = escape_parentheses(&text[name_start..name_end])?;
    Some(format!(
        "{}{fixed}{}",
        &text[..name_start],
        &text[name_end..]
    ))
}

/// Repairs a stored marketplace (`CHARGED`) mail whose item name still carries raw parentheses
/// (see the module documentation): they are rewritten to `\040` / `\041` inside the quoted title
/// argument and inside the third quoted body argument; every other byte stays. Idempotent, and
/// text that does not have the shape of our own builder is left alone. Returns whether the mail
/// changed.
pub fn repair_charged_mail(mail: &mut BaseMail) -> bool {
    if mail.mail_type != mail_type::CHARGED {
        return false;
    }
    let mut changed = false;
    if let Some(title) = repair_title(&mail.title) {
        mail.title = title;
        changed = true;
    }
    if let Some(text) = repair_body(&mail.body.text) {
        mail.body.text = text;
        changed = true;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: &str = "|gift:0;|refund:0;|limit:2100,12,31,00,00,00;";

    #[test]
    fn quote_arg_is_byte_exact() {
        assert_eq!(quote_arg("Plain Scroll"), "Plain Scroll");
        assert_eq!(
            quote_arg("Resplendent Armor Regrade Scroll (100)"),
            "Resplendent Armor Regrade Scroll \\040100\\041"
        );
        assert_eq!(
            quote_arg("Norrick's Burning Sword"),
            "Norrick\\'s Burning Sword"
        );
        assert_eq!(quote_arg("Back\\slash"), "Back\\\\slash");
        assert_eq!(
            quote_arg("Tudor Slate Villa (Veranda) Design"),
            "Tudor Slate Villa \\040Veranda\\041 Design"
        );
        // The backslash goes first: the escapes themselves are not escaped again.
        assert_eq!(quote_arg("\\(')"), "\\\\\\040\\'\\041");
        assert_eq!(quote_arg(""), "");
    }

    // --- test-only emulation of the client (see the module documentation) ---

    /// Stage A, the part both templates share: drops one leading and one trailing `'` and keeps
    /// the first `|` token (`sub_3903E6D0`, en_us).
    fn localize(buf: &str) -> String {
        let n = buf.chars().count();
        let stripped: String = buf
            .chars()
            .enumerate()
            .filter(|(i, c)| !(*c == '\'' && (*i == 0 || *i + 1 == n)))
            .map(|(_, c)| c)
            .collect();
        stripped.split('|').next().unwrap_or("").to_string()
    }

    /// Stage A of the title: `sscanf(.., "title(%[^)])")` then `"title('%s')"`.
    fn stage_a_title(text: &str) -> String {
        let Some(rest) = text.strip_prefix("title(") else {
            return text.to_string();
        };
        let arg: String = rest.chars().take_while(|c| *c != ')').collect();
        if arg.is_empty() {
            return text.to_string();
        }
        format!("title('{}')", localize(&arg))
    }

    /// Stage A of the body: `sscanf(.., "body(%[^,],%[^,],%[^)])%s") == 4` then
    /// `"body(%s,%s,'%s')%s"`.
    fn stage_a_body(text: &str) -> String {
        let Some(rest) = text.strip_prefix("body(") else {
            return text.to_string();
        };
        let scan = |s: &str, stop: char| -> (String, usize) {
            let v: String = s.chars().take_while(|c| *c != stop).collect();
            let n = v.len();
            (v, n)
        };
        let (a, n) = scan(rest, ',');
        let Some(rest) = rest.get(n + 1..).filter(|_| !a.is_empty()) else {
            return text.to_string();
        };
        let (b, n) = scan(rest, ',');
        let Some(rest) = rest.get(n + 1..).filter(|_| !b.is_empty()) else {
            return text.to_string();
        };
        let (c, n) = scan(rest, ')');
        let Some(rest) = rest.get(n + 1..).filter(|_| !c.is_empty()) else {
            return text.to_string();
        };
        let tail = rest.split_whitespace().next().unwrap_or("");
        if tail.is_empty() {
            return text.to_string();
        }
        format!("body({a},{b},'{}'){tail}", localize(&c))
    }

    /// `X2Util:ErasePipeArgsFromText` for the arguments the cash shop sends.
    fn erase_pipe_args(text: &str) -> String {
        let mut out = text.to_string();
        while let Some(i) = out.find('|') {
            let Some(end) = out[i..].find(';') else { break };
            out.replace_range(i..=i + end, "");
        }
        out
    }

    /// The Lua 5.1 lexer's `read_string` plus a flat call: `name(arg, arg, ...)` with string
    /// literals and `true`/`false`. `Err` is a `loadstring` syntax error (the UI then shows the
    /// raw text).
    fn lua_call(text: &str, name: &str) -> Result<Vec<String>, String> {
        let chars: Vec<char> = text.chars().collect();
        let head: Vec<char> = format!("{name}(").chars().collect();
        if !chars.starts_with(&head) {
            return Err("not a call".into());
        }
        let mut i = head.len();
        let mut args = Vec::new();
        loop {
            while chars.get(i) == Some(&' ') {
                i += 1;
            }
            match chars.get(i) {
                Some(&q) if q == '\'' || q == '"' => {
                    i += 1;
                    let mut s = String::new();
                    loop {
                        match chars.get(i) {
                            None | Some('\n') | Some('\r') => {
                                return Err("unfinished string".into())
                            }
                            Some(&c) if c == q => break,
                            Some('\\') => {
                                i += 1;
                                match chars.get(i) {
                                    None => return Err("unfinished string".into()),
                                    Some(d) if d.is_ascii_digit() => {
                                        let (mut n, mut k) = (0u32, 0);
                                        while k < 3
                                            && chars.get(i).is_some_and(char::is_ascii_digit)
                                        {
                                            n = n * 10 + chars[i].to_digit(10).unwrap();
                                            i += 1;
                                            k += 1;
                                        }
                                        if n > 255 {
                                            return Err("escape sequence too large".into());
                                        }
                                        s.push(char::from(n as u8));
                                        continue;
                                    }
                                    Some(&c) => s.push(match c {
                                        'n' => '\n',
                                        't' => '\t',
                                        c => c,
                                    }),
                                }
                            }
                            Some(&c) => s.push(c),
                        }
                        i += 1;
                    }
                    i += 1;
                    args.push(s);
                }
                Some(_) => {
                    let word: String = chars[i..]
                        .iter()
                        .take_while(|c| c.is_ascii_alphanumeric())
                        .collect();
                    if !["true", "false"].contains(&word.as_str()) {
                        return Err("unexpected symbol".into());
                    }
                    i += word.len();
                    args.push(word);
                }
                None => return Err("unexpected <eof>".into()),
            }
            while chars.get(i) == Some(&' ') {
                i += 1;
            }
            match chars.get(i) {
                Some(',') => i += 1,
                Some(')') => {
                    i += 1;
                    break;
                }
                _ => return Err("')' expected".into()),
            }
        }
        if i != chars.len() {
            return Err("<eof> expected".into());
        }
        Ok(args)
    }

    /// What the mailbox renders for a CHARGED mail's `title` and `body` on an en_us client.
    fn client_view(
        title: &str,
        text: &str,
    ) -> (Result<Vec<String>, String>, Result<Vec<String>, String>) {
        (
            lua_call(&stage_a_title(title), "title"),
            lua_call(&erase_pipe_args(&stage_a_body(text)), "body"),
        )
    }

    fn builder_title(arg: &str) -> String {
        format!("title('{arg}')")
    }

    fn builder_body(gifter: &str, arg: &str, gift: bool) -> String {
        let present = if gift && !gifter.is_empty() {
            "true"
        } else {
            "false"
        };
        let gift = if gift { 1 } else { 0 };
        format!(
            "body({present},'{gifter}','{arg}')|gift:{gift};|refund:0;|limit:2100,12,31,00,00,00;"
        )
    }

    const NAMES: [&str; 6] = [
        "Plain Scroll",
        "Resplendent Armor Regrade Scroll (100)",
        "Norrick's Burning Sword",
        "Tudor Slate Villa (Veranda) Design",
        "Back\\slash (x)",
        "(100)(2) 'a' \\'",
    ];

    #[test]
    fn escaped_names_survive_both_client_stages() {
        for name in NAMES {
            let q = quote_arg(name);
            let (title, body) = client_view(&builder_title(&q), &builder_body("", &q, false));
            assert_eq!(title, Ok(vec![name.to_string()]), "title of {name:?}");
            assert_eq!(
                body,
                Ok(vec!["false".to_string(), String::new(), name.to_string()]),
                "body of {name:?}"
            );
            // A gift keeps the gifter and the pipe arguments.
            let gifter = quote_arg("Alice");
            let (_, body) = client_view(&builder_title(&q), &builder_body(&gifter, &q, true));
            assert_eq!(
                body,
                Ok(vec![
                    "true".to_string(),
                    "Alice".to_string(),
                    name.to_string()
                ]),
                "gift body of {name:?}"
            );
        }
    }

    #[test]
    fn unescaped_parentheses_are_cut_by_stage_a() {
        // The behaviour seen on the real client: the title stops at the first `)`, the body is a
        // Lua syntax error.
        let name = "Resplendent Armor Regrade Scroll (100)";
        let (title, body) = client_view(&builder_title(name), &builder_body("", name, false));
        assert_eq!(
            title,
            Ok(vec!["Resplendent Armor Regrade Scroll (100".to_string()])
        );
        assert!(body.is_err());
    }

    #[test]
    fn repair_rewrites_legacy_charged_mails() {
        let mut mail = BaseMail::new();
        mail.mail_type = mail_type::CHARGED;
        mail.title = "title('Resplendent Armor Regrade Scroll (100)')".into();
        mail.body.text = format!("body(false,'','Resplendent Armor Regrade Scroll (100)'){LIMIT}");
        assert!(repair_charged_mail(&mut mail));
        assert_eq!(
            mail.title,
            "title('Resplendent Armor Regrade Scroll \\040100\\041')"
        );
        assert_eq!(
            mail.body.text,
            format!("body(false,'','Resplendent Armor Regrade Scroll \\040100\\041'){LIMIT}")
        );
        // Idempotent.
        let once = mail.clone();
        assert!(!repair_charged_mail(&mut mail));
        assert_eq!(mail, once);
        // And the result is what the client renders.
        assert_eq!(
            client_view(&mail.title, &mail.body.text),
            (
                Ok(vec!["Resplendent Armor Regrade Scroll (100)".to_string()]),
                Ok(vec![
                    "false".to_string(),
                    String::new(),
                    "Resplendent Armor Regrade Scroll (100)".to_string()
                ])
            )
        );
    }

    #[test]
    fn repair_keeps_quotes_gifter_and_everything_else() {
        let mut mail = BaseMail::new();
        mail.mail_type = mail_type::CHARGED;
        mail.title = "title('Bob\\'s (Taffy)')".into();
        mail.body.text =
            "body(true,'Alice','Bob\\'s (Taffy)')|gift:1;|refund:0;|limit:2100,12,31,00,00,00;"
                .into();
        assert!(repair_charged_mail(&mut mail));
        assert_eq!(mail.title, "title('Bob\\'s \\040Taffy\\041')");
        assert_eq!(
            mail.body.text,
            "body(true,'Alice','Bob\\'s \\040Taffy\\041')|gift:1;|refund:0;|limit:2100,12,31,00,00,00;"
        );
        // Only the title needs a repair (the body was written by a newer server).
        let mut half = BaseMail::new();
        half.mail_type = mail_type::CHARGED;
        half.title = "title('X (1)')".into();
        half.body.text = format!("body(false,'','X \\0401\\041'){LIMIT}");
        assert!(repair_charged_mail(&mut half));
        assert_eq!(half.title, "title('X \\0401\\041')");
        assert_eq!(
            half.body.text,
            format!("body(false,'','X \\0401\\041'){LIMIT}")
        );
    }

    #[test]
    fn repair_ignores_other_mails_and_foreign_shapes() {
        let legacy_title = "title('X (100)')";
        let legacy_body = format!("body(false,'','X (100)'){LIMIT}");
        // Not a marketplace mail.
        for ty in [
            mail_type::NORMAL,
            mail_type::PROMOTION,
            mail_type::AUC_BID_WIN,
        ] {
            let mut mail = BaseMail::new();
            mail.mail_type = ty;
            mail.title = legacy_title.into();
            mail.body.text = legacy_body.clone();
            assert!(!repair_charged_mail(&mut mail));
            assert_eq!(
                (mail.title.as_str(), mail.body.text.as_str()),
                (legacy_title, legacy_body.as_str())
            );
        }
        // CHARGED, but not our builder's shape.
        for (title, text) in [
            ("title(25,'Villa (1)')", "body('Villa (1)', 2, 3)"),
            ("Hello (world)", "Hello (world)"),
            ("title('X (1)'", "body(false,'','X (1)')"),
            ("title()", "body(maybe,'','X (1)')|gift:0;"),
            ("title('')", "body(true,'x')|gift:1;|limit:1;"),
        ] {
            let mut mail = BaseMail::new();
            mail.mail_type = mail_type::CHARGED;
            mail.title = title.into();
            mail.body.text = text.into();
            assert!(!repair_charged_mail(&mut mail), "{title:?} / {text:?}");
            assert_eq!(
                (mail.title.as_str(), mail.body.text.as_str()),
                (title, text)
            );
        }
    }
}
