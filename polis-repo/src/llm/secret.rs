//! The API key: read from the environment, and kept out of everything else.
//!
//! # The rule this module exists to make mechanical
//!
//! A key must never reach a file, a log line, an error message, a test fixture,
//! a commit, a process argument list or a rendered image. Those are seven
//! separate discipline problems if the key is a `String`, and one type problem
//! if it is not — so it is not.
//!
//! [`Secret`] therefore:
//!
//! * has no `Serialize`, so it cannot be written into a cache or a config;
//! * has no `Display`, so it cannot be interpolated into a message by accident;
//! * prints as `Secret(<redacted>)` under `Debug`, so a `{:?}` of any struct
//!   that holds one — an [`crate::llm::HttpRequest`], a whole runner — is safe
//!   to log;
//! * is reachable only through [`Secret::expose`], which is one greppable name.
//!
//! [`scrub`] is the belt to that pair of braces: any text on its way into an
//! error — a subprocess's stderr, a provider's echoed request — has the key
//! removed from it first, so even a channel nobody thought about cannot carry
//! one.

use std::fmt;

/// An API key, in memory and nowhere else.
///
/// Constructed only from the environment. There is deliberately no
/// `from_str`, no `new(&str)` and no `Deserialize`: a key that can be
/// constructed from a literal is a key that ends up in a test fixture.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// The first of `names` that is set to something non-empty.
    ///
    /// `None` is a normal outcome and the shipped default state — see
    /// [`crate::llm::LlmConfig::key`].
    pub fn from_env(names: &[String]) -> Option<Self> {
        for name in names {
            if let Some(value) = std::env::var_os(name) {
                let value = value.to_string_lossy().trim().to_owned();
                if !value.is_empty() {
                    return Some(Self(value));
                }
            }
        }
        None
    }

    /// The key itself. **The only way out of this type.**
    ///
    /// Call sites: building an `Authorization` header, and nothing else.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// How long it is. Safe to print, and enough to tell "set" from "set to
    /// something truncated" in a bug report.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when the key is empty, which [`Secret::from_env`] cannot produce.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Removes `secret` from `text`, wherever it appears.
///
/// For text arriving from somewhere this crate does not control — a
/// subprocess's stderr, a provider's error body that echoes the request — on its
/// way into an [`crate::llm::LlmError`]. Cheap, and the cost of not doing it is
/// a key in a log file.
pub fn scrub(text: &str, secret: Option<&Secret>) -> String {
    let Some(secret) = secret else {
        return text.to_owned();
    };
    if secret.is_empty() {
        return text.to_owned();
    }
    text.replace(secret.expose(), "<redacted>")
}

/// Bounds a message from an untrusted source and strips what must never be
/// drawn or logged.
///
/// Control characters and the Unicode bidi overrides go, because an error
/// message reaches a terminal and a status bar; the result is capped so a
/// provider that returns an HTML error page does not become a log entry.
pub fn bound_message(text: &str, max_chars: usize) -> String {
    // Both families become a space rather than vanishing: a log line is read by
    // a person, and `limitedreversed` hides that two words were joined.
    // `is_unsafe_char` is `describe`'s list, so there is one definition of
    // "must not be rendered" rather than two.
    let cleaned: String = text
        .chars()
        .map(|c| {
            if crate::describe::is_unsafe_char(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    let cut = collapsed
        .char_indices()
        .nth(max_chars)
        .map_or(collapsed.len(), |(i, _)| i);
    let mut out = collapsed[..cut].to_owned();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A struct holding a key, to prove a `{:?}` of one is safe to log.
    #[derive(Debug)]
    struct Holder {
        #[allow(dead_code)]
        key: Secret,
    }

    /// The only place in the workspace that may build a `Secret` from a
    /// literal, and it does it through the environment so the production path
    /// is the one under test.
    fn with_env<T>(name: &str, value: &str, body: impl FnOnce() -> T) -> T {
        // SAFETY-adjacent: `set_var` is process-global. These tests do not run
        // concurrently with anything that reads the same name because the name
        // is unique per test.
        std::env::set_var(name, value);
        let out = body();
        std::env::remove_var(name);
        out
    }

    #[test]
    fn a_secret_never_prints_itself() {
        let secret = with_env("POLIS_TEST_KEY_A", "sk-not-a-real-key-000", || {
            Secret::from_env(&["POLIS_TEST_KEY_A".to_owned()]).expect("set")
        });
        let debug = format!("{secret:?}");
        assert_eq!(debug, "Secret(<redacted>)");
        assert!(!debug.contains("sk-"), "{debug}");
        // And inside a struct, which is how it actually gets logged.
        let wrapped = format!("{:?}", Holder { key: secret });
        assert!(!wrapped.contains("sk-"), "{wrapped}");
    }

    #[test]
    fn the_first_set_variable_wins_and_blank_ones_do_not_count() {
        let names = [
            "POLIS_TEST_KEY_B1".to_owned(),
            "POLIS_TEST_KEY_B2".to_owned(),
        ];
        assert!(Secret::from_env(&names).is_none(), "neither is set");
        with_env("POLIS_TEST_KEY_B2", "second", || {
            assert_eq!(Secret::from_env(&names).expect("b2").expose(), "second");
        });
        with_env("POLIS_TEST_KEY_B1", "   ", || {
            with_env("POLIS_TEST_KEY_B2", "second", || {
                assert_eq!(
                    Secret::from_env(&names).expect("b2").expose(),
                    "second",
                    "a blank variable is not a key"
                );
            });
        });
        assert!(Secret::from_env(&[]).is_none(), "no names, no key");
    }

    #[test]
    fn scrub_removes_the_key_from_text_that_came_from_elsewhere() {
        let secret = with_env("POLIS_TEST_KEY_C", "abc123def456", || {
            Secret::from_env(&["POLIS_TEST_KEY_C".to_owned()]).expect("set")
        });
        let stderr = "curl: (22) HTTP 401 for Bearer abc123def456 at api.example";
        let scrubbed = scrub(stderr, Some(&secret));
        assert!(!scrubbed.contains("abc123def456"), "{scrubbed}");
        assert!(scrubbed.contains("<redacted>"), "{scrubbed}");
        assert_eq!(scrub(stderr, None), stderr, "no key, nothing to remove");
    }

    #[test]
    fn a_message_from_a_provider_is_bounded_and_cannot_carry_control_codes() {
        let hostile = "rate\u{0007}limited\u{202E}reversed\n\n  exceeded";
        let bounded = bound_message(hostile, 200);
        assert!(!bounded.chars().any(char::is_control), "{bounded:?}");
        assert!(!bounded.contains('\u{202E}'), "{bounded:?}");
        assert_eq!(bounded, "rate limited reversed exceeded");
        let long = "x".repeat(500);
        let bounded = bound_message(&long, 40);
        assert_eq!(bounded.chars().count(), 41, "40 plus the ellipsis");
        assert!(bounded.ends_with('…'));
    }
}
