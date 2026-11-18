//! BERT uncased WordPiece, the `BertTokenizer` that `all-MiniLM-L6-v2` ships
//! with (`do_lower_case = true`, `tokenize_chinese_chars = true`, accents
//! stripped because lowercasing is on).
//!
//! It follows Hugging Face's `BasicTokenizer` then `WordpieceTokenizer`:
//! drop NUL, U+FFFD and control characters, split on whitespace, put spaces
//! around CJK ideographs, lowercase, NFD and drop nonspacing marks, split on
//! punctuation, then greedy longest-match pieces with a `##` prefix. A word
//! longer than 100 characters, or one with no piece match, is `[UNK]`.
//!
//! Tokenizing stops as soon as `max_len` ids are filled, so a large body does
//! not pay for text the model would truncate anyway.

use std::collections::HashMap;

use unicode_normalization::UnicodeNormalization;
use unicode_properties::{GeneralCategory, GeneralCategoryGroup, UnicodeGeneralCategory};

const MAX_WORD_CHARS: usize = 100;

pub(crate) struct WordPiece {
    vocab: HashMap<String, i64>,
    cls: i64,
    sep: i64,
    unk: i64,
}

impl WordPiece {
    /// One token per line. The line number is the id. `None` when the vocab
    /// lacks `[CLS]`, `[SEP]` or `[UNK]`.
    pub(crate) fn from_vocab(text: &str) -> Option<Self> {
        let mut vocab = HashMap::new();
        for (index, line) in text.lines().enumerate() {
            let id = i64::try_from(index).ok()?;
            vocab
                .entry(line.trim_end_matches('\r').to_string())
                .or_insert(id);
        }
        Some(Self {
            cls: *vocab.get("[CLS]")?,
            sep: *vocab.get("[SEP]")?,
            unk: *vocab.get("[UNK]")?,
            vocab,
        })
    }

    /// `[CLS] pieces… [SEP]`, at most `max_len` ids (and at least the two
    /// markers). Pieces past the limit are dropped, the way the model card's
    /// `max_seq_length` truncates.
    pub(crate) fn encode(&self, text: &str, max_len: usize) -> Vec<i64> {
        let room = max_len.max(2) - 2;
        let mut ids = Vec::with_capacity(room.min(64) + 2);
        ids.push(self.cls);
        basic_words(text, |word| {
            for id in self.pieces(word) {
                if ids.len() > room {
                    return false;
                }
                ids.push(id);
            }
            true
        });
        ids.push(self.sep);
        ids
    }

    fn pieces(&self, word: &str) -> Vec<i64> {
        let chars: Vec<char> = word.chars().collect();
        if chars.len() > MAX_WORD_CHARS {
            return vec![self.unk];
        }
        let mut out = Vec::new();
        let mut piece = String::new();
        let mut start = 0;
        while start < chars.len() {
            let mut end = chars.len();
            let mut found = None;
            while start < end {
                piece.clear();
                if start > 0 {
                    piece.push_str("##");
                }
                piece.extend(&chars[start..end]);
                if let Some(id) = self.vocab.get(piece.as_str()) {
                    found = Some(*id);
                    break;
                }
                end -= 1;
            }
            let Some(id) = found else {
                return vec![self.unk];
            };
            out.push(id);
            start = end;
        }
        out
    }
}

/// Calls `word` for each basic token in order until it returns `false`.
fn basic_words(text: &str, mut word: impl FnMut(&str) -> bool) {
    let mut current = String::new();
    macro_rules! emit {
        ($token:expr) => {
            if !word($token) {
                return;
            }
        };
    }
    for ch in text.chars() {
        if ch == '\0' || ch == '\u{fffd}' || is_control(ch) {
            continue;
        }
        if is_whitespace(ch) {
            if !current.is_empty() {
                emit!(&current);
                current.clear();
            }
            continue;
        }
        if is_cjk(ch) {
            if !current.is_empty() {
                emit!(&current);
                current.clear();
            }
            let mut buf = [0u8; 4];
            emit!(ch.encode_utf8(&mut buf));
            continue;
        }
        for lower in ch.to_lowercase() {
            for base in std::iter::once(lower).nfd() {
                if base.general_category() == GeneralCategory::NonspacingMark {
                    continue;
                }
                if is_punctuation(base) {
                    if !current.is_empty() {
                        emit!(&current);
                        current.clear();
                    }
                    let mut buf = [0u8; 4];
                    emit!(base.encode_utf8(&mut buf));
                } else {
                    current.push(base);
                }
            }
        }
    }
    if !current.is_empty() {
        word(&current);
    }
}

fn is_whitespace(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r')
        || ch.general_category() == GeneralCategory::SpaceSeparator
}

/// Hugging Face treats every `C*` category as control, except tab, newline
/// and carriage return.
fn is_control(ch: char) -> bool {
    !matches!(ch, '\t' | '\n' | '\r') && ch.general_category_group() == GeneralCategoryGroup::Other
}

fn is_punctuation(ch: char) -> bool {
    let cp = u32::from(ch);
    (33..=47).contains(&cp)
        || (58..=64).contains(&cp)
        || (91..=96).contains(&cp)
        || (123..=126).contains(&cp)
        || ch.general_category_group() == GeneralCategoryGroup::Punctuation
}

fn is_cjk(ch: char) -> bool {
    let cp = u32::from(ch);
    (0x4E00..=0x9FFF).contains(&cp)
        || (0x3400..=0x4DBF).contains(&cp)
        || (0x20000..=0x2A6DF).contains(&cp)
        || (0x2A700..=0x2B73F).contains(&cp)
        || (0x2B740..=0x2B81F).contains(&cp)
        || (0x2B820..=0x2CEAF).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0x2F800..=0x2FA1F).contains(&cp)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VOCAB: &str =
        "[PAD]\n[UNK]\n[CLS]\n[SEP]\nthe\ninvoice\n##s\nun\n##aff\n##able\n.\n,\ncafe\n中\n!";

    fn tok() -> WordPiece {
        WordPiece::from_vocab(VOCAB).unwrap()
    }

    #[test]
    fn splits_like_bert_uncased() {
        let tok = tok();
        // [CLS] the invoice ##s , unaffable . cafe 中 [UNK] ! [SEP]
        assert_eq!(
            tok.encode("The INVOICEs, unaffable.\tCafé中 xyz!", 64),
            vec![2, 4, 5, 6, 11, 7, 8, 9, 10, 12, 13, 1, 14, 3]
        );
    }

    #[test]
    fn drops_control_and_replacement_chars() {
        let tok = tok();
        assert_eq!(
            tok.encode("the\u{0}\u{fffd}\u{200b}invoice", 8),
            vec![2, 1, 3]
        );
        assert_eq!(tok.encode("the\u{7}", 8), vec![2, 4, 3]);
        assert_eq!(tok.encode(" \n\t", 8), vec![2, 3]);
    }

    #[test]
    fn truncates_to_max_len_and_keeps_markers() {
        let tok = tok();
        let ids = tok.encode(&"the invoice ".repeat(1000), 5);
        assert_eq!(ids, vec![2, 4, 5, 4, 3]);
        assert_eq!(tok.encode("the invoice", 0), vec![2, 3]);
    }

    #[test]
    fn a_long_word_is_unknown() {
        let tok = tok();
        assert_eq!(tok.encode(&"a".repeat(101), 8), vec![2, 1, 3]);
    }

    #[test]
    fn a_vocab_without_markers_is_rejected() {
        assert!(WordPiece::from_vocab("the\ninvoice").is_none());
    }
}
