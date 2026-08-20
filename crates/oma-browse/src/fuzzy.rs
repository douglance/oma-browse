//! fzf-style fuzzy matching.
//!
//! The palette searches three corpora at once -- open tabs, every command, and
//! browsing history -- and they have to be ranked against each other, so they
//! all have to be scored the same way. A plain subsequence test is not enough:
//! it ranked `nav back` above `nav go` for the query "go", because `nav back`'s
//! description happens to begin "Go back in history".
//!
//! This follows fzf's scoring model: a match is worth a fixed amount, gaps cost,
//! and position earns bonuses -- most at the start of a word, then at a
//! camelCase hump, then for simply continuing the previous match. That is what
//! makes `gh` find `GitHub` and `enwiki` find `en.wikipedia.org`.

/// A match is worth this much before any bonus.
const MATCH: i32 = 16;
/// Opening a gap between matched characters.
const GAP_START: i32 = -3;
/// Each further character of that gap.
const GAP_EXTENSION: i32 = -1;
/// First character of a word: after a separator, or the very beginning.
const BONUS_BOUNDARY: i32 = MATCH / 2;
/// A lowercase-to-uppercase hump, or a letter after a digit.
const BONUS_CAMEL: i32 = BONUS_BOUNDARY - 1;
/// Directly after the previous match.
const BONUS_CONSECUTIVE: i32 = -(GAP_START + GAP_EXTENSION);
/// Matching the very first character is worth more than matching a later one,
/// which is what makes a prefix beat a match buried in the middle.
const BONUS_FIRST_MULTIPLIER: i32 = 2;

fn is_separator(c: char) -> bool {
    matches!(c, ' ' | '/' | '-' | '_' | '.' | ':' | ',' | '?' | '&' | '=' | '#' | '+' | '\'' | '"')
}

/// What a match at `index` is worth on top of [`MATCH`], given what precedes it.
fn bonus_at(chars: &[char], index: usize) -> i32 {
    if index == 0 {
        return BONUS_BOUNDARY;
    }
    let prev = chars[index - 1];
    let cur = chars[index];
    if is_separator(prev) {
        BONUS_BOUNDARY
    // Both of these are the same kind of edge -- a case change and a
    // digit-to-letter change each start a new word without a separator.
    } else if (prev.is_lowercase() && cur.is_uppercase())
        || (prev.is_ascii_digit() && cur.is_alphabetic())
    {
        BONUS_CAMEL
    } else {
        0
    }
}

/// Score `needle` against `haystack`, or `None` when it does not match.
///
/// `needle` must already be lowercase; matching is case-insensitive and the
/// haystack is folded per character rather than allocated a second time.
///
/// Greedy rather than fzf's full dynamic-programming pass: it takes the earliest
/// match for each needle character, then re-runs from the last match's start to
/// catch the common case where a later, tighter run scores better ("wiki"
/// against "en.wikipedia.org" should land on the word, not the stray `w`).
pub fn score(haystack: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let chars: Vec<char> = haystack.chars().collect();
    let first = forward(&chars, needle, 0)?;
    // Retry anchored past the first matched character: if the same needle also
    // matches later, the later run is usually the tighter one.
    let better = forward(&chars, needle, first.0 + 1);
    let best = match better {
        Some(second) if second.1 > first.1 => second.1,
        _ => first.1,
    };
    // Shorter haystacks win ties, so an exact command name beats a long
    // description that happens to contain the same letters.
    Some(best - (chars.len() as i32) / 16)
}

/// One greedy pass. Returns the index the match started at and its score.
fn forward(chars: &[char], needle: &str, from: usize) -> Option<(usize, i32)> {
    let mut score = 0;
    let mut start = None;
    let mut prev_match: Option<usize> = None;
    // The bonus the current run of consecutive matches began with.
    let mut run_bonus = 0;
    let mut at = from;

    for want in needle.chars() {
        let found = chars[at..]
            .iter()
            .position(|c| c.to_ascii_lowercase() == want)
            .map(|offset| at + offset)?;

        let mut bonus = bonus_at(chars, found);
        match prev_match {
            None => {
                start = Some(found);
                run_bonus = bonus;
                bonus *= BONUS_FIRST_MULTIPLIER;
            }
            Some(prev) if found == prev + 1 => {
                // Inside a run, every character inherits the bonus the run
                // started with. Without this, "z o o m" outscores "zoom" for
                // the query "zoom": each of its letters follows a space and so
                // earns a word-start bonus, while the letters of the real word
                // earn only the smaller consecutive bonus.
                if bonus >= BONUS_BOUNDARY && bonus > run_bonus {
                    run_bonus = bonus;
                }
                bonus = bonus.max(run_bonus).max(BONUS_CONSECUTIVE);
            }
            Some(prev) => {
                run_bonus = bonus;
                let gap = (found - prev - 1) as i32;
                score += GAP_START + GAP_EXTENSION * (gap - 1);
            }
        }
        score += MATCH + bonus;
        prev_match = Some(found);
        at = found + 1;
    }
    Some((start?, score))
}

#[cfg(test)]
mod tests {
    use super::score;

    fn best<'a>(needle: &str, options: &[&'a str]) -> &'a str {
        options
            .iter()
            .filter_map(|o| score(o, needle).map(|s| (s, *o)))
            .max_by_key(|(s, _)| *s)
            .map(|(_, o)| o)
            .unwrap_or("")
    }

    #[test]
    fn a_word_start_beats_a_letter_in_the_middle() {
        assert!(score("go back in history", "go") < score("nav go", "go"));
    }

    #[test]
    fn initials_find_a_host() {
        assert_eq!(
            best("ycom", &["en.wikipedia.org", "news.ycombinator.com"]),
            "news.ycombinator.com"
        );
        assert_eq!(best("wiki", &["news.ycombinator.com", "en.wikipedia.org"]), "en.wikipedia.org");
    }

    #[test]
    fn consecutive_beats_scattered() {
        assert!(score("zoom", "zoom") > score("z o o m", "zoom"));
    }

    #[test]
    fn a_miss_is_none() {
        assert_eq!(score("page zoom", "xyzzy"), None);
        assert_eq!(score("", "a"), None);
    }

    #[test]
    fn an_empty_needle_matches_everything() {
        assert_eq!(score("anything at all", ""), Some(0));
    }

    #[test]
    fn shorter_wins_a_tie() {
        // Both contain "reload" at a word start; the terser one should win.
        assert!(
            score("reload", "reload")
                > score("reload the active tab after a very long explanation", "reload")
        );
    }
}
