pub(crate) fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    text.get(..text.floor_char_boundary(max_bytes))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_utf8_rounds_down_to_a_char_boundary() {
        // 'é' is two bytes: a cut inside it must round down, not panic.
        assert_eq!(truncate_utf8("aé", 2), "a");
        assert_eq!(truncate_utf8("aé", 3), "aé");
        assert_eq!(truncate_utf8("abc", 10), "abc");
        assert_eq!(truncate_utf8("", 5), "");
    }
}
