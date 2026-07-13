pub mod cargo;
pub mod curl;
pub mod discord;
pub mod docker;
pub mod generic_cli;
pub mod gh;
pub mod git;
pub mod npm;
pub mod pytest;
pub mod shellcheck;
pub mod x07;

/// Largest char boundary `<= max` in `s`. Call this before byte-slicing
/// `&s[..n]` when capping tool output: `String::from_utf8_lossy` output can
/// contain multi-byte UTF-8, and a raw byte offset that lands mid-character
/// panics. `std::str::floor_char_boundary` would do this but is unstable on the
/// pinned stable toolchain, so we implement it here.
pub(crate) fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::floor_char_boundary;

    #[test]
    fn floor_char_boundary_never_splits_a_char() {
        // "€" is 3 bytes (E2 82 AC). A cap at byte 1 or 2 must floor to 0.
        let s = "€uro";
        assert_eq!(floor_char_boundary(s, 1), 0);
        assert_eq!(floor_char_boundary(s, 2), 0);
        assert_eq!(floor_char_boundary(s, 3), 3); // boundary after '€'
        assert!(s.is_char_boundary(floor_char_boundary(s, 2)));
        // Slicing at the returned offset must not panic.
        let _ = &s[..floor_char_boundary(s, 2)];
    }

    #[test]
    fn floor_char_boundary_caps_at_len() {
        let s = "abc";
        assert_eq!(floor_char_boundary(s, 99), 3);
        assert_eq!(floor_char_boundary(s, 3), 3);
    }
}
