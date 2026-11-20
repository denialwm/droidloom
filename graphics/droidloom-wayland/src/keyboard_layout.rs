//! The host keyboard's layout, as Android names its physical keyboard layouts.
//!
//! A compositor compiles its keymap from RMLVO names, and the keymap keeps the
//! symbols they chose as its `xkb_symbols` name: `pc+it+us(intl):2+inet(evdev)`,
//! or with `_` for `+` and `:` in newer libxkbcommon. Each group's layout gives
//! the language tag and layout type of an input method subtype's physical
//! keyboard hint, from which Android picks the matching layout of its own.

/// An Android physical keyboard hint: a BCP 47 language tag and a layout type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Hint {
    pub language: &'static str,
    pub kind: &'static str,
}

/// Android's own layout, used for a layout it has none for.
const US: Hint = hint("en-Latn-US", "qwerty");

const fn hint(language: &'static str, kind: &'static str) -> Hint {
    Hint { language, kind }
}

/// The hint for each group of the keymap, by group index.
pub(super) fn groups(keymap: &str) -> Vec<Hint> {
    let Some(name) = symbols_name(keymap) else {
        return Vec::new();
    };
    let tokens = tokens(name);
    // The model's symbols come first, then the first group's layout; each
    // further layout carries its group number.
    let mut groups = Vec::new();
    let mut rest = tokens.iter().skip(1).peekable();
    if let Some(first) = rest.next() {
        groups.push(android(first));
    }
    while let Some(layout) = rest.next() {
        let Some(group) = rest.peek().and_then(|token| token.parse::<usize>().ok()) else {
            break;
        };
        rest.next();
        if group == groups.len() + 1 {
            groups.push(android(layout));
        }
    }
    groups
}

fn symbols_name(keymap: &str) -> Option<&str> {
    let line = keymap
        .lines()
        .map(str::trim_start)
        .find(|line| line.starts_with("xkb_symbols"))?;
    let start = line.find('"')? + 1;
    let end = start + line[start..].find('"')?;
    Some(&line[start..end])
}

/// Splits at `+`, `_` and `:` outside parentheses: variants may hold `_`.
fn tokens(name: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let (mut depth, mut start) = (0_u32, 0);
    for (index, character) in name.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '+' | '_' | ':' if depth == 0 => {
                tokens.push(&name[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    tokens.push(&name[start..]);
    tokens.retain(|token| !token.is_empty());
    tokens
}

/// Android's layouts are those of its InputDevices package.
fn android(symbols: &str) -> Hint {
    let (layout, variant) = match symbols.split_once('(') {
        Some((layout, variant)) => (layout, variant.trim_end_matches(')')),
        None => (symbols, ""),
    };
    match (layout, variant) {
        ("us", v) if v.starts_with("dvorak") => hint("en-Latn-US", "dvorak"),
        ("us", v) if v.starts_with("colemak") => hint("en-Latn-US", "colemak"),
        ("us", v) if v.starts_with("workman") => hint("en-Latn-US", "workman"),
        ("us", "intl" | "alt-intl" | "altgr-intl") => hint("en-Latn-US", "extended"),
        ("us", _) => US,
        ("gb" | "ie", _) => hint("en-Latn-GB", "qwerty"),
        ("in", "eng") => hint("en-Latn-IN", "qwerty"),
        ("ca", "eng") => hint("en-Latn-CA", "qwerty"),
        ("ca", _) => hint("fr-Latn-CA", "qwerty"),
        ("de" | "at", _) => hint("de-Latn", "qwertz"),
        ("ch", v) if v.starts_with("fr") => hint("fr-Latn-CH", "qwertz"),
        ("ch", _) => hint("de-Latn-CH", "qwertz"),
        ("fr", _) => hint("fr-Latn-FR", "azerty"),
        ("be", _) => hint("fr-Latn-BE", "azerty"),
        ("it", _) => hint("it-Latn", "qwerty"),
        ("es", _) => hint("es-Latn-ES", "qwerty"),
        ("latam", _) => hint("es-Latn-419", "qwerty"),
        ("pt", _) => hint("pt-Latn-PT", "qwerty"),
        ("br", _) => hint("pt-Latn-BR", "qwerty"),
        ("dk", _) => hint("da-Latn", "qwerty"),
        ("no", _) => hint("nb-Latn", "qwerty"),
        ("se", _) => hint("sv-Latn", "qwerty"),
        ("fi", _) => hint("fi-Latn", "qwerty"),
        ("is", _) => hint("is-Latn", "qwerty"),
        ("ee", _) => hint("et-Latn", "qwerty"),
        ("lv", _) => hint("lv-Latn", "qwerty"),
        ("lt", _) => hint("lt-Latn", "qwerty"),
        ("pl", _) => hint("pl-Latn", "qwerty"),
        ("ro", _) => hint("ro-Latn-RO", "qwerty"),
        ("az", _) => hint("az-Latn-AZ", "qwerty"),
        ("hu", _) => hint("hu-Latn", "qwertz"),
        ("cz", "qwerty") => hint("cs-Latn", "qwerty"),
        ("cz", _) => hint("cs-Latn", "qwertz"),
        ("sk", "qwerty") => hint("sk-Latn", "qwerty"),
        ("sk", _) => hint("sk-Latn", "qwertz"),
        ("hr", _) => hint("hr-Latn", "qwertz"),
        ("si", _) => hint("sl-Latn", "qwertz"),
        ("rs", v) if v.starts_with("latin") => hint("sr-Latn-RS", "qwertz"),
        ("rs", _) => hint("sr-Cyrl-RS", "extended"),
        ("me", v) if v.starts_with("cyrillic") => hint("cnr-Cyrl-ME", "extended"),
        ("me", _) => hint("cnr-Latn-ME", "qwertz"),
        ("tr", "f") => hint("tr-Latn", "turkish_f"),
        ("tr", _) => hint("tr-Latn", "turkish_q"),
        ("ru", "mac") => hint("ru-Cyrl", "extended"),
        ("ru", _) => hint("ru-Cyrl", "undefined"),
        ("ua", _) => hint("uk-Cyrl", "undefined"),
        ("by", _) => hint("be-Cyrl", "undefined"),
        ("bg", "phonetic") => hint("bg-Cyrl", "extended"),
        ("bg", _) => hint("bg-Cyrl", "undefined"),
        ("mn", _) => hint("mn-Cyrl", "undefined"),
        ("gr", _) => hint("el-Grek", "undefined"),
        ("il", _) => hint("he-Hebr", "undefined"),
        ("ara", _) => hint("ar-Arab", "undefined"),
        ("ir", _) => hint("fa-Arab", "undefined"),
        ("ge", _) => hint("ka-Geor", "undefined"),
        ("th", _) => hint("th-Thai", "extended"),
        _ => US,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keymap(symbols: &str) -> String {
        format!(
            "xkb_keymap {{\nxkb_keycodes \"evdev\" {{}};\nxkb_symbols \"{symbols}\" {{\n\tname[1]=\"x\";\n}};\n}};\n"
        )
    }

    #[test]
    fn each_group_takes_its_layout_in_either_name_form() {
        for name in [
            "pc+it+us(intl):2+inet(evdev)",
            "pc_it_us(intl)_2_inet(evdev)",
        ] {
            assert_eq!(
                groups(&keymap(name)),
                [hint("it-Latn", "qwerty"), hint("en-Latn-US", "extended")]
            );
        }
    }

    #[test]
    fn options_are_not_layouts() {
        assert_eq!(
            groups(&keymap(
                "pc_us_inet(evdev)_compose(ralt)_group(alt_shift_toggle)"
            )),
            [US]
        );
        assert_eq!(
            groups(&keymap("pc_fr(oss_latin9)_inet(evdev)")),
            [hint("fr-Latn-FR", "azerty")]
        );
    }

    #[test]
    fn variants_choose_the_layout() {
        assert_eq!(
            groups(&keymap("pc_ch(fr)_inet(evdev)")),
            [hint("fr-Latn-CH", "qwertz")]
        );
        assert_eq!(
            groups(&keymap("pc+de(nodeadkeys)+us(dvorak):2+rs(latin):3")),
            [
                hint("de-Latn", "qwertz"),
                hint("en-Latn-US", "dvorak"),
                hint("sr-Latn-RS", "qwertz"),
            ]
        );
    }

    #[test]
    fn a_layout_android_lacks_types_as_us_and_no_name_gives_nothing() {
        assert_eq!(groups(&keymap("pc_kr_inet(evdev)")), [US]);
        assert!(groups("xkb_keymap {\n};\n").is_empty());
    }
}
