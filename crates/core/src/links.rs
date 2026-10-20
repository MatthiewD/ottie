//! Links in text someone pasted or imported (a list, an email, a web page),
//! for adding several downloads at once (docs/decisions/0036), and the
//! sequences in them (0037).

use std::collections::HashSet;
use std::path::PathBuf;

use reqwest::Url;

use crate::category::Matched;
use crate::download::check_scheme;
use crate::error::Error;
use crate::sequence::Sequence;

/// Something in the text that looks like a link (`scheme://…`).
#[derive(Debug)]
pub enum Found {
    Link(Url),
    /// It can't be downloaded: `error` is [`Error::InvalidUrl`] or
    /// [`Error::UnsupportedScheme`].
    Invalid {
        text: String,
        error: Error,
    },
}

/// A link found, and what adding it would do.
#[derive(Debug)]
pub enum LinkPreview {
    Link {
        url: Url,
        /// The file name the link suggests; the server may give another.
        name: String,
        /// The category it would get without one chosen.
        matched: Matched,
        /// A download of this same link is already on the list.
        known: bool,
        /// The folder it would be saved in, if it has one.
        folder: Option<PathBuf>,
        /// A file named `name` is already in that folder (docs/decisions/0049).
        exists: bool,
    },
    Invalid {
        text: String,
        error: Error,
    },
}

/// The most text the links of one paste may add up to: past this, long
/// links in a sequence would take memory by the gigabyte.
const MAX_BYTES: usize = 64 * 1024 * 1024;

/// Every link in `text`, in order, each once: anything of the form
/// `scheme://…` up to a space, a quote or an angle bracket, without the
/// punctuation that ends a sentence around it. A sequence in a link gives
/// each link it holds (docs/decisions/0037). [`Error::TooManyLinks`] if
/// there would be more than `max`, or more than [`MAX_BYTES`] of them (then
/// `max` is how many of them would fit), counted before making them.
pub fn find(text: &str, max: usize) -> Result<Vec<Found>, Error> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let (mut count, mut bytes): (usize, usize) = (0, 0);
    for candidate in candidates_in(text) {
        if seen.insert(candidate) {
            let sequence = Sequence::parse(candidate);
            count = count.saturating_add(sequence.count());
            // No link of a sequence is longer than the sequence as written.
            bytes = bytes.saturating_add(sequence.count().saturating_mul(candidate.len()));
            candidates.push((candidate, sequence));
        }
    }
    if count > max {
        return Err(Error::TooManyLinks { count, max });
    }
    if bytes > MAX_BYTES {
        let fit = MAX_BYTES as u128 * count as u128 / bytes as u128;
        let max = usize::try_from(fit).unwrap_or(usize::MAX);
        return Err(Error::TooManyLinks { count, max });
    }
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for (candidate, sequence) in &candidates {
        expand(candidate, sequence, &mut found, &mut seen);
    }
    Ok(found)
}

/// What looks like a link in `text`, as written, in order.
fn candidates_in(text: &str) -> Vec<&str> {
    let mut candidates = Vec::new();
    let mut rest = 0;
    while let Some(at) = text[rest..].find("://") {
        let separator = rest + at;
        // The scheme is the letters just before.
        let start = text[..separator]
            .trim_end_matches(|c: char| c.is_ascii_alphabetic())
            .len();
        // "://" is no boundary, so this is past it.
        let end = text[separator..]
            .find(is_boundary)
            .map_or(text.len(), |len| separator + len);
        rest = end;
        if start != separator {
            candidates.push(trim_end(&text[start..end]));
        }
    }
    candidates
}

/// Each link of a sequence (just one, without the escapes, if it holds
/// none). If its scheme can't be downloaded, it's reported once, as written.
fn expand(
    candidate: &str,
    sequence: &Sequence,
    found: &mut Vec<Found>,
    seen: &mut HashSet<String>,
) {
    for (n, link) in sequence.links().enumerate() {
        let one = check(&link);
        if n == 0
            && let Found::Invalid {
                error: error @ Error::UnsupportedScheme(_),
                ..
            } = one
        {
            // The scheme is the same in every link (no sequence before "://").
            push_once(invalid(candidate, error), found, seen);
            return;
        }
        push_once(one, found, seen);
    }
}

fn check(link: &str) -> Found {
    match Url::parse(link) {
        Ok(url) => match check_scheme(&url) {
            Ok(()) => Found::Link(url),
            Err(error) => invalid(link, error),
        },
        Err(_) => invalid(link, Error::InvalidUrl),
    }
}

fn push_once(one: Found, found: &mut Vec<Found>, seen: &mut HashSet<String>) {
    let key = match &one {
        Found::Link(url) => url.as_str(),
        Found::Invalid { text, .. } => text.as_str(),
    };
    if seen.insert(key.to_owned()) {
        found.push(one);
    }
}

fn invalid(text: &str, error: Error) -> Found {
    Found::Invalid {
        text: text.to_owned(),
        error,
    }
}

/// Ends a link: links can't hold these as they are (they're percent-encoded).
fn is_boundary(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '<' | '>' | '`')
}

/// Without what likely belongs to the sentence around it: a final full stop,
/// comma or quote, or a closing bracket the link doesn't open itself (as in
/// "(see https://example.com/a)", but not "https://en.wikipedia.org/wiki/Otter_(disambiguation)").
/// The brackets are counted once, then kept up as they go, so a long run of
/// them takes linear time.
fn trim_end(link: &str) -> &str {
    let mut counts = [0usize; BRACKETS.len()];
    for c in link.chars() {
        if let Some(i) = BRACKETS.iter().position(|&b| b == c) {
            counts[i] += 1;
        }
    }
    let mut link = link;
    while let Some(last) = link.chars().next_back() {
        let bracket = BRACKETS.iter().position(|&b| b == last);
        let trailing = match bracket {
            // A closing one (odd index): more of them than of its opening one.
            Some(i) if i % 2 == 1 => counts[i - 1] < counts[i],
            Some(_) => false,
            None => matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\''),
        };
        if !trailing {
            break;
        }
        if let Some(i) = bracket {
            counts[i] -= 1;
        }
        link = &link[..link.len() - last.len_utf8()];
    }
    link
}

/// Each opening bracket, then its closing one.
const BRACKETS: [char; 6] = ['(', ')', '[', ']', '{', '}'];

#[cfg(test)]
mod tests {
    use super::*;

    /// The links found, as text; invalid ones as "invalid: …".
    fn found(text: &str) -> Vec<String> {
        find(text, usize::MAX)
            .unwrap()
            .into_iter()
            .map(|found| match found {
                Found::Link(url) => url.to_string(),
                Found::Invalid { text, error } => format!("invalid: {text} ({error})"),
            })
            .collect()
    }

    #[test]
    fn one_link_per_line() {
        let text = "https://example.com/a.zip\r\nhttp://example.com/b.iso\n\n  https://example.com/c d.mp4\n";
        assert_eq!(
            found(text),
            [
                "https://example.com/a.zip",
                "http://example.com/b.iso",
                "https://example.com/c",
            ]
        );
    }

    #[test]
    fn links_in_prose_lose_the_punctuation_around_them() {
        let text = "Get https://example.com/a.zip, then (see https://example.com/b.zip). \
                    Also \"https://example.com/c.zip\" and <https://example.com/d.zip>! \
                    Or [this](https://example.com/e.zip): 'https://example.com/f.zip'.";
        assert_eq!(
            found(text),
            [
                "https://example.com/a.zip",
                "https://example.com/b.zip",
                "https://example.com/c.zip",
                "https://example.com/d.zip",
                "https://example.com/e.zip",
                "https://example.com/f.zip",
            ]
        );
    }

    #[test]
    fn brackets_the_link_opens_stay() {
        assert_eq!(
            found("(https://en.wikipedia.org/wiki/Otter_(disambiguation))"),
            ["https://en.wikipedia.org/wiki/Otter_(disambiguation)"]
        );
        assert_eq!(
            found("http://[::1]:8080/file.bin"),
            ["http://[::1]:8080/file.bin"]
        );
    }

    #[test]
    fn links_in_html() {
        let text =
            r#"<a href="https://example.com/a.zip">A</a><img src="https://example.com/b.png?w=2">"#;
        assert_eq!(
            found(text),
            ["https://example.com/a.zip", "https://example.com/b.png?w=2"]
        );
    }

    #[test]
    fn each_link_once_the_first_time() {
        let text = "https://example.com/a.zip HTTPS://EXAMPLE.COM/a.zip https://example.com/b.zip https://example.com/a.zip";
        assert_eq!(
            found(text),
            ["https://example.com/a.zip", "https://example.com/b.zip"]
        );
    }

    #[test]
    fn links_that_cant_be_downloaded_are_reported() {
        assert_eq!(
            found(
                "sftp://example.com/a.zip http://exa%mple.com/ ://nothing https:// ftp://example.com/pub/"
            ),
            [
                "invalid: sftp://example.com/a.zip (unsupported URL scheme `sftp` (only http, https, ftp, ftps and ftpes are supported))",
                "invalid: http://exa%mple.com/ (not a valid link)",
                "invalid: https:// (not a valid link)",
                "invalid: ftp://example.com/pub/ (not a valid link)",
            ]
        );
    }

    #[test]
    fn ftp_links_count() {
        assert_eq!(
            found("ftp://nas.local/a.iso ftps://me:pw@nas.local/b.iso ftpes://nas.local/c.iso"),
            [
                "ftp://nas.local/a.iso",
                "ftps://me:pw@nas.local/b.iso",
                "ftpes://nas.local/c.iso"
            ]
        );
    }

    #[test]
    fn the_scheme_is_the_letters_just_before() {
        assert_eq!(
            found("Link:https://example.com/a.zip é.http://example.com/b.zip"),
            ["https://example.com/a.zip", "http://example.com/b.zip"]
        );
    }

    #[test]
    fn a_sequence_gives_each_link() {
        assert_eq!(
            found(
                "Pages https://example.com/p[1-3].jpg, then https://img{1,2}.example.com/{a,b}.png."
            ),
            [
                "https://example.com/p1.jpg",
                "https://example.com/p2.jpg",
                "https://example.com/p3.jpg",
                "https://img1.example.com/a.png",
                "https://img1.example.com/b.png",
                "https://img2.example.com/a.png",
                "https://img2.example.com/b.png",
            ]
        );
        // Each once, a sequence or not.
        assert_eq!(
            found("https://example.com/[1-2] https://example.com/1 https://example.com/{2,1,3}"),
            [
                "https://example.com/1",
                "https://example.com/2",
                "https://example.com/3",
            ]
        );
    }

    #[test]
    fn what_isnt_a_sequence_stays_and_escapes_go() {
        assert_eq!(
            found(r"http://[::1]/a[]=1&b[0]=2&c={id} https://example.com/a\[1-2\].jpg"),
            [
                "http://[::1]/a[]=1&b[0]=2&c=%7Bid%7D",
                "https://example.com/a[1-2].jpg",
            ]
        );
    }

    #[test]
    fn a_sequence_that_cant_be_downloaded_is_reported_once() {
        assert_eq!(
            found("sftp://example.com/[1-100].zip"),
            [
                "invalid: sftp://example.com/[1-100].zip (unsupported URL scheme `sftp` (only http, https, ftp, ftps and ftpes are supported))"
            ]
        );
    }

    #[test]
    fn too_many_are_counted_before_making_them() {
        let text = "https://example.com/[1-3] https://example.com/[1-3] https://example.com/{a,b}[0-999999999999]";
        assert!(matches!(
            find(text, 1000),
            Err(Error::TooManyLinks { count, max: 1000 }) if count > 1_000_000_000_000
        ));
        // Each link as written counts once.
        assert!(matches!(
            find(
                "https://example.com/[1-3] https://example.com/[1-3] https://example.com/x",
                3
            ),
            Err(Error::TooManyLinks { count: 4, max: 3 })
        ));
        assert_eq!(
            find("https://example.com/[1-3] https://example.com/[1-3]", 3)
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn no_links() {
        assert!(found("").is_empty());
        assert!(found("nothing to see, not even example.com/a.zip").is_empty());
    }

    #[test]
    fn long_runs_of_brackets_are_read_in_linear_time() {
        for filler in [")", "]", "}", "[", "{"] {
            let text = format!("https://example.com/{}", filler.repeat(100_000));
            let start = std::time::Instant::now();
            let _ = find(&text, usize::MAX);
            let took = start.elapsed();
            assert!(
                took < std::time::Duration::from_secs(1),
                "{filler}: {took:?}"
            );
        }
    }

    #[test]
    fn long_links_in_a_sequence_are_refused_before_they_are_made() {
        let text = format!("https://example.com/{}/[1-10000].bin", "x".repeat(1 << 20));
        let start = std::time::Instant::now();
        let Err(Error::TooManyLinks { count, max }) = find(&text, 10_000) else {
            panic!("expected too many links");
        };
        assert_eq!(count, 10_000);
        // About 64 of these 1 MiB links fit.
        assert!((60..=64).contains(&max), "{max}");
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        // Short ones still go up to the count.
        assert_eq!(
            find("https://example.com/[1-10000].bin", 10_000)
                .unwrap()
                .len(),
            10_000
        );
    }
}
