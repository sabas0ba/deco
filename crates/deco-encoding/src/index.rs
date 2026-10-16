//! The Encoding Standard's index tables, read from the copies in
//! `data/whatwg/` the first time each is needed.
//!
//! An index maps a pointer, a number the legacy encoding's bytes compute,
//! to a code point. The files are kept exactly as published, so their
//! provenance can be checked against the identifier and hash recorded in
//! `data/whatwg/README.md`; parsing them once at run time costs about a
//! millisecond, and only for a file that needs them.

use std::collections::HashMap;
use std::sync::OnceLock;

/// One index: code point by pointer, and the first pointer for each code
/// point, as the standard's "index pointer" is defined.
pub struct Index {
    by_pointer: Vec<Option<char>>,
    by_code_point: HashMap<char, u16>,
}

impl Index {
    fn parse(source: &str) -> Self {
        let mut entries: Vec<(usize, char)> = Vec::new();
        for line in source.lines() {
            let line = line.trim_start();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.split('\t');
            let (Some(pointer), Some(code_point)) = (fields.next(), fields.next()) else {
                continue;
            };
            let pointer: usize = pointer
                .trim()
                .parse()
                .expect("an index pointer is a number");
            let code_point = u32::from_str_radix(code_point.trim().trim_start_matches("0x"), 16)
                .ok()
                .and_then(char::from_u32)
                .expect("an index code point is a scalar value");
            entries.push((pointer, code_point));
        }
        let len = entries
            .iter()
            .map(|(pointer, _)| pointer + 1)
            .max()
            .unwrap_or(0);
        let mut by_pointer = vec![None; len];
        let mut by_code_point = HashMap::with_capacity(entries.len());
        for (pointer, code_point) in entries {
            by_pointer[pointer] = Some(code_point);
            // The first pointer wins, as the standard's index pointer does.
            by_code_point
                .entry(code_point)
                .or_insert(u16::try_from(pointer).expect("an index pointer fits in 16 bits"));
        }
        Self {
            by_pointer,
            by_code_point,
        }
    }

    /// The code point for `pointer`, if the index has one.
    pub fn code_point(&self, pointer: usize) -> Option<char> {
        self.by_pointer.get(pointer).copied().flatten()
    }

    /// The first pointer for `code_point`, if any.
    pub fn pointer(&self, code_point: char) -> Option<usize> {
        self.by_code_point
            .get(&code_point)
            .map(|&pointer| usize::from(pointer))
    }

    /// How many pointers have a code point.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.by_pointer.iter().flatten().count()
    }
}

macro_rules! index {
    ($name:ident, $file:literal) => {
        pub fn $name() -> &'static Index {
            static INDEX: OnceLock<Index> = OnceLock::new();
            INDEX.get_or_init(|| Index::parse(include_str!(concat!("../data/whatwg/", $file))))
        }
    };
}

index!(jis0208, "index-jis0208.txt");
index!(jis0212, "index-jis0212.txt");
index!(windows1252, "index-windows-1252.txt");

/// The index Shift_JIS encodes with: index jis0208 without the pointers
/// 8272 to 8835, which duplicate characters encoded elsewhere (the NEC and
/// IBM extensions), so the first pointer for each character is the one the
/// standard's Shift_JIS encoder uses.
pub fn shift_jis_pointer(code_point: char) -> Option<usize> {
    static POINTERS: OnceLock<HashMap<char, u16>> = OnceLock::new();
    POINTERS
        .get_or_init(|| {
            let index = jis0208();
            let mut pointers = HashMap::new();
            for (pointer, code_point) in index.by_pointer.iter().enumerate() {
                if (8272..=8835).contains(&pointer) {
                    continue;
                }
                if let Some(code_point) = code_point {
                    pointers.entry(*code_point).or_insert(pointer as u16);
                }
            }
            pointers
        })
        .get(&code_point)
        .map(|&pointer| usize::from(pointer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_indexes_hold_what_the_standard_publishes() {
        // Entry counts of the files as published on 2024-09-18.
        assert_eq!(jis0208().len(), 7724);
        assert_eq!(jis0212().len(), 6067);
        assert_eq!(windows1252().len(), 128);
        assert_eq!(jis0208().code_point(0), Some('\u{3000}'));
        assert_eq!(jis0208().pointer('、'), Some(1));
        assert_eq!(windows1252().code_point(0), Some('€'));
    }
}
