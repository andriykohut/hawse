pub const MAX_LEN: usize = 32;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("name is empty")]
    Empty,
    #[error("name is {0} characters, the limit is 32")]
    TooLong(usize),
    #[error("name cannot start with `{0}`")]
    Start(char),
    #[error("name contains `{0}`; only a-z, 0-9 and - are allowed")]
    Char(char),
}

pub fn validate(name: &str) -> Result<(), NameError> {
    let mut chars = name.chars();
    let first = chars.next().ok_or(NameError::Empty)?;
    if name.len() > MAX_LEN {
        return Err(NameError::TooLong(name.len()));
    }
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(if first == '-' {
            NameError::Start(first)
        } else {
            NameError::Char(first)
        });
    }
    if let Some(bad) = chars.find(|c| !c.is_ascii_lowercase() && !c.is_ascii_digit() && *c != '-') {
        return Err(NameError::Char(bad));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_lowercase_digits_and_dashes() {
        for ok in ["web", "p3000", "andrii-mbp", "a", &"x".repeat(32)] {
            assert_eq!(validate(ok), Ok(()), "{ok}");
        }
    }

    #[test]
    fn rejects_bad_names() {
        assert_eq!(validate(""), Err(NameError::Empty));
        assert_eq!(validate("-web"), Err(NameError::Start('-')));
        assert_eq!(validate("Web"), Err(NameError::Char('W')));
        assert_eq!(validate("my_svc"), Err(NameError::Char('_')));
        assert_eq!(validate(&"x".repeat(33)), Err(NameError::TooLong(33)));
    }
}
