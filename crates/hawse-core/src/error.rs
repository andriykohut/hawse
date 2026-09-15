/// Joins the error and each `source()` under it with `": "`:
/// `connection failed: invalid peer certificate: UnknownIssuer`.
pub(crate) fn chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("outer")]
    struct Outer(#[source] Inner);

    #[derive(Debug, thiserror::Error)]
    #[error("inner")]
    struct Inner;

    #[test]
    fn every_cause_is_joined() {
        assert_eq!(chain(&Outer(Inner)), "outer: inner");
        assert_eq!(chain(&Inner), "inner");
    }
}
