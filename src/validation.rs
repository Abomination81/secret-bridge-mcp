/// Checks new text before it reaches a dialog or the registry.
pub(crate) fn validate_display_text(
    name: &str,
    value: &str,
    min: usize,
    max: usize,
    allow_newlines: bool,
) -> Result<(), String> {
    validate_text(name, value, min, max, allow_newlines, |character| {
        is_unsafe_display_character(character) || is_hidden_layout_character(character)
    })
}

/// Checks text already stored by an earlier release, which applied only the original character
/// rules. Rejecting such records would make the whole registry unreadable after an upgrade.
pub(crate) fn validate_stored_text(
    name: &str,
    value: &str,
    min: usize,
    max: usize,
    allow_newlines: bool,
) -> Result<(), String> {
    validate_text(
        name,
        value,
        min,
        max,
        allow_newlines,
        is_unsafe_display_character,
    )
}

fn validate_text(
    name: &str,
    value: &str,
    min: usize,
    max: usize,
    allow_newlines: bool,
    unsafe_character: impl Fn(char) -> bool,
) -> Result<(), String> {
    if value.trim().len() < min || value.len() > max {
        return Err(format!("{name} must be between {min} and {max} bytes"));
    }
    if value.chars().any(|character| {
        (character.is_control() && !(allow_newlines && matches!(character, '\n' | '\t')))
            || unsafe_character(character)
    }) {
        return Err(format!(
            "{name} contains a control, directional, or invisible formatting character"
        ));
    }
    Ok(())
}

pub(crate) fn valid_secret_id(id: &str) -> bool {
    id.len() == 35
        && id.starts_with("sb_")
        && id[3..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Directional and invisible formatting characters, refused since the first release.
fn is_unsafe_display_character(character: char) -> bool {
    matches!(
        character,
        '\u{00ad}'
            | '\u{034f}'
            | '\u{061c}'
            | '\u{115f}'
            | '\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{ffa0}'
            | '\u{e0100}'..='\u{e01ef}'
    )
}

/// Refused in new text since 0.4: line and paragraph separators, which AppKit renders as line
/// breaks that can push real dialog text out of view, and other invisible format characters.
fn is_hidden_layout_character(character: char) -> bool {
    matches!(
        character,
        '\u{0600}'..='\u{0605}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{08e2}'
            | '\u{2028}'..='\u{2029}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0000}'..='\u{e007f}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_directional_and_invisible_spoofing() {
        assert!(validate_display_text("label", "safe label", 3, 120, false).is_ok());
        assert!(validate_display_text("label", "safe\u{202e}txt", 3, 120, false).is_err());
        assert!(validate_display_text("label", "api\u{200b}key", 3, 120, false).is_err());
    }

    #[test]
    fn records_from_earlier_releases_stay_readable() {
        // A flag emoji uses tag characters, which 0.3 accepted in descriptions.
        let flag = "UK key \u{1f3f4}\u{e0067}\u{e0062}\u{e0065}\u{e006e}\u{e0067}\u{e007f}";
        assert!(validate_display_text("description", flag, 3, 500, true).is_err());
        assert!(validate_stored_text("description", flag, 3, 500, true).is_ok());
        assert!(validate_stored_text("label", "safe\u{202e}txt", 3, 120, false).is_err());
    }

    #[test]
    fn rejects_separators_and_tag_characters_even_where_newlines_are_allowed() {
        for spoof in [
            "app\u{2028}\u{2028}x",
            "app\u{2029}x",
            "app\u{e0041}x",
            "app\u{fffa}x",
        ] {
            assert!(validate_display_text("path", spoof, 1, 4096, false).is_err());
            assert!(validate_display_text("description", spoof, 1, 500, true).is_err());
        }
        assert!(validate_display_text("description", "line one\nline two", 3, 500, true).is_ok());
    }
}
