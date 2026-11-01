//! Unicode properties used by WrongStack regexes with the `u` flag.

use unicode_general_category::{GeneralCategory, get_general_category};

/// `\p{L}`: Lu, Ll, Lt, Lm, Lo.
pub fn is_letter(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::UppercaseLetter
            | GeneralCategory::LowercaseLetter
            | GeneralCategory::TitlecaseLetter
            | GeneralCategory::ModifierLetter
            | GeneralCategory::OtherLetter
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn letters_exclude_marks_and_letter_numbers() {
        assert!(super::is_letter('é'));
        assert!(super::is_letter('語'));
        assert!(!super::is_letter('\u{0301}'), "combining mark");
        assert!(!super::is_letter('Ⅻ'), "Nl");
        assert!(!super::is_letter('1'));
    }
}
