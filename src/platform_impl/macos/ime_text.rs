//! Rune: what `Ime::Preedit` can't carry, beside it. The app's text around its selection, for
//! `selectedRange` and `attributedSubstringForProposedRange:` (reconversion), a composition's
//! clauses from `setMarkedText:`'s attributes, and the range a reconversion replaces.

use std::cell::RefCell;
use std::ops::Range;

type Clauses = Vec<(Range<usize>, bool)>;

thread_local! {
    /// The focused field's paragraph and its selection, UTF-8 bytes.
    pub(crate) static TEXT: RefCell<Option<(String, Range<usize>)>> = const { RefCell::new(None) };
    /// The last marked text and its clauses (byte ranges, whether each is being converted).
    pub(crate) static CLAUSES: RefCell<Option<(String, Clauses)>> = const { RefCell::new(None) };
    /// The range of `TEXT` a reconversion's marked text replaces, UTF-8 bytes.
    pub(crate) static REPLACEMENT: RefCell<Option<Range<usize>>> = const { RefCell::new(None) };
}

/// `utf16` UTF-16 units into `text` as a UTF-8 byte offset, clamped to its end.
pub(crate) fn byte_of(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (i, c) in text.char_indices() {
        if units >= utf16 {
            return i;
        }
        units += c.len_utf16();
    }
    text.len()
}

/// UTF-8 byte `byte` of `text` in UTF-16 units, clamped down to a character boundary.
pub(crate) fn utf16_of(text: &str, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    text[..byte].encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::macos::{set_ime_text, take_ime_clauses, take_ime_replacement};

    #[test]
    fn unicode_offsets_and_malformed_bytes() {
        let text = "a日😀b";
        for (byte, units) in [(0, 0), (1, 1), (4, 2), (8, 4), (9, 5)] {
            assert_eq!(utf16_of(text, byte), units);
            assert_eq!(byte_of(text, units), byte);
        }
        assert_eq!(utf16_of(text, 2), 1);
        assert_eq!(utf16_of(text, 7), 2);
        assert_eq!(utf16_of(text, usize::MAX), 5);
        // Rune rounds a split surrogate up to the next character boundary.
        assert_eq!(byte_of(text, 3), 8);
        assert_eq!(byte_of(text, usize::MAX), text.len());
        assert_eq!(byte_of("", usize::MAX), 0);
        assert_eq!(utf16_of("", usize::MAX), 0);
    }

    #[test]
    fn publication_and_side_channels_are_consumed_as_rune_expects() {
        set_ime_text(Some(("日本".into(), 0..6)));
        TEXT.with(|t| assert_eq!(*t.borrow(), Some(("日本".into(), 0..6))));
        set_ime_text(None);
        TEXT.with(|t| assert!(t.borrow().is_none()));

        CLAUSES.with(|c| *c.borrow_mut() = Some(("日本".into(), vec![(0..6, true)])));
        assert_eq!(take_ime_clauses("日本"), vec![(0..6, true)]);
        assert!(take_ime_clauses("日本").is_empty());
        CLAUSES.with(|c| *c.borrow_mut() = Some(("日本".into(), vec![(0..6, true)])));
        assert!(take_ime_clauses("other").is_empty());
        assert!(take_ime_clauses("日本").is_empty());

        REPLACEMENT.with(|r| *r.borrow_mut() = Some(1..7));
        assert_eq!(take_ime_replacement(), Some(1..7));
        assert_eq!(take_ime_replacement(), None);
    }

    #[test]
    fn publication_and_pending_results_are_thread_local() {
        set_ime_text(Some(("parent".into(), 1..2)));
        CLAUSES.with(|c| *c.borrow_mut() = Some(("parent".into(), vec![(0..6, false)])));
        REPLACEMENT.with(|r| *r.borrow_mut() = Some(1..2));
        std::thread::spawn(|| {
            TEXT.with(|t| assert!(t.borrow().is_none()));
            assert!(take_ime_clauses("parent").is_empty());
            assert_eq!(take_ime_replacement(), None);
            set_ime_text(Some(("child".into(), 0..5)));
        })
        .join()
        .unwrap();
        TEXT.with(|t| assert_eq!(*t.borrow(), Some(("parent".into(), 1..2))));
        assert_eq!(take_ime_clauses("parent"), vec![(0..6, false)]);
        assert_eq!(take_ime_replacement(), Some(1..2));
        set_ime_text(None);
    }
}
