pub(crate) fn validate_display_text(
    name: &str,
    value: &str,
    min: usize,
    max: usize,
    allow_newlines: bool,
) -> Result<(), String> {
    if value.trim().len() < min || value.len() > max {
        return Err(format!("{name} must be between {min} and {max} bytes"));
    }
    if value.chars().any(|character| {
        (character.is_control() && !(allow_newlines && matches!(character, '\n' | '\t')))
            || is_unsafe_display_character(character)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_directional_and_invisible_spoofing() {
        assert!(validate_display_text("label", "safe label", 3, 120, false).is_ok());
        assert!(validate_display_text("label", "safe\u{202e}txt", 3, 120, false).is_err());
        assert!(validate_display_text("label", "api\u{200b}key", 3, 120, false).is_err());
    }
}
