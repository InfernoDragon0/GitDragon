//! Parsers for git's machine-readable output. Pure functions over bytes: no I/O, no panics on
//! malformed or huge input (fuzzed, see `crates/git-core/fuzz`).
//!
//! Each module exports the exact arguments its parser expects (`ARGS` / `FORMAT`), so
//! [`crate::CliRepo`] and the parser can never drift apart.

pub mod diff;
pub mod log;
pub mod refs;
pub mod status;

/// Malformed git output. Carries the byte offset where parsing gave up.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot parse git {what} output at byte {offset}: {reason}")]
pub struct ParseError {
    pub what: &'static str,
    pub offset: usize,
    pub reason: String,
}

impl ParseError {
    pub fn new(what: &'static str, offset: usize, reason: impl Into<String>) -> Self {
        Self {
            what,
            offset,
            reason: reason.into(),
        }
    }
}

pub type ParseResult<T> = Result<T, ParseError>;

/// Splits `input` into `sep`-terminated records, yielding `(offset, record)`. A last record
/// without a terminator is kept; nothing is yielded for the empty tail after a final `sep`.
fn records(input: &[u8], sep: u8) -> impl Iterator<Item = (usize, &[u8])> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        let rest = input.get(pos..).filter(|rest| !rest.is_empty())?;
        let start = pos;
        let len = memchr::memchr(sep, rest).unwrap_or(rest.len());
        pos = start + len + 1;
        Some((start, &rest[..len]))
    })
}

/// Unsigned decimal without sign or whitespace; `None` on overflow or any other byte.
fn parse_uint<T: TryFrom<u64>>(digits: &[u8]) -> Option<T> {
    if digits.is_empty() {
        return None;
    }
    let n = digits.iter().try_fold(0u64, |n, &d| {
        if !d.is_ascii_digit() {
            return None;
        }
        n.checked_mul(10)?.checked_add(u64::from(d - b'0'))
    })?;
    T::try_from(n).ok()
}

/// Signed decimal with an optional leading `-`.
fn parse_i64(text: &[u8]) -> Option<i64> {
    match text.strip_prefix(b"-") {
        Some(digits) => {
            let n: u64 = parse_uint(digits)?;
            0i64.checked_sub_unsigned(n)
        }
        None => parse_uint(text),
    }
}

/// Full hex object id (SHA-1 or SHA-256).
fn parse_oid(hex: &[u8]) -> Option<String> {
    let valid = matches!(hex.len(), 40 | 64) && hex.iter().all(u8::is_ascii_hexdigit);
    valid.then(|| String::from_utf8_lossy(hex).into_owned())
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
