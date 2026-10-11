//! What the greeter says, and its tests.
//!
//! The development loop's gate changes [`WORDS`] on Ferrix, builds and tests
//! again, and requires the running service to say the new words after its
//! restart. The tests do not name the words, so they pass before and after.

/// What the service calls itself by. The gate's edit is to this line.
pub const WORDS: &str = "version one";

/// The line the service answers every connection with.
#[must_use]
pub fn greeting() -> String {
    format!("greeter: hello from {WORDS}")
}

/// The `index`th word of [`WORDS`].
///
/// # Panics
///
/// When there are not that many words.
#[must_use]
pub fn word(index: usize) -> &'static str {
    WORDS
        .split(' ')
        .nth(index)
        .unwrap_or_else(|| panic!("no such word: {index}"))
}

#[cfg(test)]
mod tests {
    use super::{greeting, word};

    #[test]
    fn the_greeting_names_the_program_and_its_words() {
        assert!(greeting().starts_with("greeter: hello from version "));
        assert_eq!(word(0), "version");
    }

    /// A test that panics is a test that unwinds: on a C library whose
    /// unwinder finds no tables this ends the whole test program with
    /// `SIGABRT`, as it did on ferrousli until 2026-10-11.
    #[test]
    #[should_panic(expected = "no such word: 7")]
    fn a_word_past_the_end_panics() {
        let _ = word(7);
    }

    /// What rust-analyzer does to cancel a query: unwind to a caller that
    /// catches it, and go on.
    #[test]
    fn a_caught_panic_leaves_its_caller_running() {
        let caught = std::panic::catch_unwind(|| word(7));
        assert!(caught.is_err());
        assert_eq!(word(1).len(), 3);
    }
}
