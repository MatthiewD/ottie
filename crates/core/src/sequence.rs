//! Sequences in one link, curl-style (docs/decisions/0037): `[001-120]`,
//! `[a-z]`, `[0-100:10]`, `{small,large}`. Several in one link give every
//! combination. Only well-formed ones count: anything else stays as written,
//! and `\[`, `\]`, `\{`, `\}` (and `\,` in a list) are the character itself.

/// A link as written, split into fixed text and the sequences in it.
#[derive(Debug, PartialEq, Eq)]
pub struct Sequence {
    parts: Vec<Part>,
}

#[derive(Debug, PartialEq, Eq)]
enum Part {
    Text(String),
    /// `[first-last:step]`, padded with zeros to `width` digits.
    Numbers {
        first: u64,
        last: u64,
        step: u64,
        width: usize,
    },
    /// `[a-z:step]`, both in the same case.
    Letters {
        first: u8,
        last: u8,
        step: u64,
    },
    /// `{a,b,c}`: at least two items (`{id}` stays as written).
    List(Vec<String>),
}

impl Sequence {
    pub fn parse(link: &str) -> Self {
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut rest = link;
        while let Some(c) = rest.chars().next() {
            let sequence = match c {
                '[' => range(rest),
                '{' => list(rest),
                _ => None,
            };
            if let Some((part, len)) = sequence {
                if !text.is_empty() {
                    parts.push(Part::Text(std::mem::take(&mut text)));
                }
                parts.push(part);
                rest = &rest[len..];
                continue;
            }
            match escaped(rest) {
                Some(c) => {
                    text.push(c);
                    rest = &rest[2..];
                }
                None => {
                    text.push(c);
                    rest = &rest[c.len_utf8()..];
                }
            }
        }
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        Self { parts }
    }

    /// How many links it gives, without making them (`usize::MAX` if more).
    pub fn count(&self) -> usize {
        self.parts
            .iter()
            .fold(1, |count, part| count.saturating_mul(part.count()))
    }

    /// Every link it gives, in order: the last sequence changes fastest.
    pub fn links(&self) -> impl Iterator<Item = String> + '_ {
        let total = self.count();
        // The index in each part, like the digits of an odometer.
        let mut at = vec![0; self.parts.len()];
        (0..total).map(move |n| {
            let link = self
                .parts
                .iter()
                .zip(&at)
                .fold(String::new(), |mut link, (part, &i)| {
                    part.push(i, &mut link);
                    link
                });
            if n + 1 < total {
                for (i, part) in at.iter_mut().zip(&self.parts).rev() {
                    *i += 1;
                    if *i < part.count() {
                        break;
                    }
                    *i = 0;
                }
            }
            link
        })
    }
}

impl Part {
    fn count(&self) -> usize {
        match self {
            Part::Text(_) => 1,
            Part::Numbers {
                first, last, step, ..
            } => (first.abs_diff(*last) / step)
                // `[0-18446744073709551615]` holds one more than a u64 can count.
                .checked_add(1)
                .and_then(|count| usize::try_from(count).ok())
                .unwrap_or(usize::MAX),
            Part::Letters { first, last, step } => {
                // At most 26.
                (u64::from(first.abs_diff(*last)) / step + 1) as usize
            }
            Part::List(items) => items.len(),
        }
    }

    /// Its `i`th value (`i` < `count()`) after `link`.
    fn push(&self, i: usize, link: &mut String) {
        match self {
            Part::Text(text) => link.push_str(text),
            Part::Numbers {
                first,
                last,
                step,
                width,
            } => {
                let value = nth(*first, *last, *step, i);
                link.push_str(&format!("{value:0width$}"));
            }
            Part::Letters { first, last, step } => {
                let value = nth(u64::from(*first), u64::from(*last), *step, i);
                // Between two letters.
                link.push(char::from(value as u8));
            }
            Part::List(items) => {
                if let Some(item) = items.get(i) {
                    link.push_str(item);
                }
            }
        }
    }
}

/// The `i`th value from `first` to `last` by `step`, counting down if
/// `last` is below `first`. `i` < the count, so it lies between the two.
fn nth(first: u64, last: u64, step: u64, i: usize) -> u64 {
    let offset = i as u64 * step;
    if last < first {
        first - offset
    } else {
        first + offset
    }
}

/// `\[`, `\]`, `\{`, `\}` at the start of `text`: that character.
fn escaped(text: &str) -> Option<char> {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some('\\'), Some(c @ ('[' | ']' | '{' | '}'))) => Some(c),
        _ => None,
    }
}

/// The longest a range can be written: two 20-digit numbers (`u64`), a
/// step, `[`, `-`, `:` and `]`. Looking no further for its `]` keeps a link
/// full of `[` from being read again from each one.
const MAX_RANGE: usize = 64;

/// `[first-last]` or `[first-last:step]` at the start of `text` (which
/// starts with `[`), and its length.
fn range(text: &str) -> Option<(Part, usize)> {
    // `]` is ASCII: its byte index is where it starts.
    let len = text.bytes().take(MAX_RANGE).position(|b| b == b']')? + 1;
    let inside = &text[1..len - 1];
    let (bounds, step) = match inside.split_once(':') {
        Some((bounds, step)) => (bounds, Some(step)),
        None => (inside, None),
    };
    let (first, last) = bounds.split_once('-')?;
    let step = match step {
        Some(step) if is_number(step) => step.parse::<u64>().ok().filter(|&step| step > 0)?,
        Some(_) => return None,
        None => 1,
    };
    if is_number(first) && is_number(last) {
        // A leading zero pads them all to the first one's width, as curl does.
        let width = if first.len() > 1 && first.starts_with('0') {
            first.len()
        } else {
            0
        };
        let part = Part::Numbers {
            first: first.parse().ok()?,
            last: last.parse().ok()?,
            step,
            width,
        };
        return Some((part, len));
    }
    let (&[first], &[last]) = (first.as_bytes(), last.as_bytes()) else {
        return None;
    };
    let same_case = (first.is_ascii_lowercase() && last.is_ascii_lowercase())
        || (first.is_ascii_uppercase() && last.is_ascii_uppercase());
    same_case.then_some((Part::Letters { first, last, step }, len))
}

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// `{a,b,…}` at the start of `text` (which starts with `{`), and its length.
/// Items may be empty; `\,` and the escapes are the character itself; no
/// brackets or braces otherwise.
fn list(text: &str) -> Option<(Part, usize)> {
    let mut items = vec![String::new()];
    let mut chars = text.char_indices().skip(1);
    while let Some((at, c)) = chars.next() {
        match c {
            '}' if items.len() > 1 => return Some((Part::List(items), at + 1)),
            ',' => items.push(String::new()),
            '\\' => {
                let next = chars.next().map(|(_, c)| c);
                let item = items.last_mut()?;
                match next {
                    Some(c @ (',' | '[' | ']' | '{' | '}')) => item.push(c),
                    Some(c) => {
                        item.push('\\');
                        item.push(c);
                    }
                    None => return None,
                }
            }
            '[' | ']' | '{' | '}' => return None,
            c => items.last_mut()?.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(link: &str) -> Vec<String> {
        Sequence::parse(link).links().collect()
    }

    #[test]
    fn numbers_padded_as_written() {
        assert_eq!(links("a/[1-3].jpg"), ["a/1.jpg", "a/2.jpg", "a/3.jpg"]);
        assert_eq!(links("a/[08-10]"), ["a/08", "a/09", "a/10"]);
        assert_eq!(links("[001-120]").len(), 120);
        assert_eq!(links("[001-120]")[119], "120");
        assert_eq!(links("[098-100]"), ["098", "099", "100"]);
        assert_eq!(links("[9-11]"), ["9", "10", "11"]);
    }

    #[test]
    fn steps_and_counting_down() {
        assert_eq!(links("[0-100:25]"), ["0", "25", "50", "75", "100"]);
        assert_eq!(links("[0-10:4]"), ["0", "4", "8"]);
        assert_eq!(links("[3-1]"), ["3", "2", "1"]);
        assert_eq!(links("[10-0:5]"), ["10", "5", "0"]);
        assert_eq!(links("[a-g:3]"), ["a", "d", "g"]);
        assert_eq!(links("[C-A]"), ["C", "B", "A"]);
    }

    #[test]
    fn letters_and_lists() {
        assert_eq!(links("[a-c]"), ["a", "b", "c"]);
        assert_eq!(links("[X-Z]"), ["X", "Y", "Z"]);
        assert_eq!(links("img{,_hd}.png"), ["img.png", "img_hd.png"]);
        assert_eq!(links("{small,medium,large}"), ["small", "medium", "large"]);
    }

    #[test]
    fn every_combination_the_last_changing_fastest() {
        assert_eq!(
            links("s[1-2]/{a,b}[x-y]"),
            [
                "s1/ax", "s1/ay", "s1/bx", "s1/by", "s2/ax", "s2/ay", "s2/bx", "s2/by"
            ]
        );
    }

    #[test]
    fn only_well_formed_sequences_count() {
        for link in [
            "http://[::1]:8080/a",
            "http://[2001:db8::1]/a",
            "a[]=1&b[0]=2",
            "[1-]",
            "[-1]",
            "[1-3:0]",
            "[1-3:x]",
            "[a-Z]",
            "[aa-zz]",
            "[1-c]",
            "{id}",
            "{}",
            "{a,b",
            "{a,b{}",
            "[1-2",
        ] {
            assert_eq!(links(link), [link], "{link}");
        }
    }

    #[test]
    fn escapes_are_the_character_itself() {
        assert_eq!(links(r"a\[1-2\]"), ["a[1-2]"]);
        assert_eq!(links(r"\{a,b\}"), ["{a,b}"]);
        assert_eq!(links(r"{a\,b,c}"), ["a,b", "c"]);
        assert_eq!(links(r"{a\x,b}"), [r"a\x", "b"]);
        assert_eq!(links(r"a\b"), [r"a\b"]);
    }

    #[test]
    fn counting_without_making_them() {
        let huge = Sequence::parse("[0-999999999999][0-999999999999]");
        assert_eq!(huge.count(), usize::MAX);
        assert_eq!(Sequence::parse("[1-20]{a,b,c}[a-e]").count(), 300);
        assert_eq!(Sequence::parse("plain").count(), 1);
        assert_eq!(links("plain"), ["plain"]);
    }

    #[test]
    fn counting_every_u64_saturates() {
        for link in [
            "https://example.test/[0-18446744073709551615]",
            "https://example.test/[18446744073709551615-0]",
        ] {
            assert_eq!(Sequence::parse(link).count(), usize::MAX, "{link}");
        }
    }
}
