//! Derive a one-line **byline** for a clipping's list row from its frontmatter. In the real vault
//! `web link` is near-universal but `author` is usually empty, so the byline is publisher-first: a
//! curated domain→name map covers the common outlets and blogs, then a non-empty `author`, then a
//! person-looking `category`, then a prettified domain. Computed once at scan time (stored on
//! [`crate::model::VaultFile`]) and also matched by library search and the author filter.
//!
//! **A publisher is not a platform.** For an outlet the masthead *is* the identity, so `nytimes.com`
//! outranks whichever journalist happens to be credited. For a social host the domain says only
//! where a post lives, so it must lose to a named author — see [`platform_for_host`]. Until
//! 2026-08-27 both lived in one map, and every X and YouTube clipping was attributed to "X" or
//! "YouTube" instead of the person who wrote it.

use crate::parse::Meta;

/// Best display name for "who published this", or `None` when nothing usable is present.
pub fn derive_byline(meta: &Meta) -> Option<String> {
    let host = meta.web_link.as_deref().and_then(host_of);

    // 1. A known publisher / blog, keyed off the web-link host — the strongest, most-present signal.
    //    Outlets only: a platform host deliberately does NOT match here (see `platform_for_host`).
    if let Some(h) = &host {
        if let Some(name) = publisher_for_host(h) {
            return Some(name.to_string());
        }
    }
    // 2. A non-empty author (the ~50 clippings with a real `author:`).
    if let Some(a) = meta.author.as_deref() {
        let a = a.trim();
        if !a.is_empty() {
            return Some(a.to_string());
        }
    }
    // 3. A category that reads like a person's name (skips topic categories like "AI"/"diet").
    if let Some(c) = meta.categories.iter().find(|c| looks_like_person(c)) {
        return Some(c.trim().to_string());
    }
    // 4. The outlet the clipper recorded in `publisher:`. Below the author on purpose — a named
    //    human is the better byline — but well above a guessed domain: this is the difference
    //    between "Financial Times" and "Ft", "CIDRAP" and "Umn".
    if let Some(pubr) = meta.publisher.as_deref() {
        let pubr = pubr.trim();
        if !pubr.is_empty() && !is_platform_name(pubr) {
            return Some(pubr.to_string());
        }
    }
    // 5. The platform a post lives on — only now that nothing has named a human or an outlet.
    //    Ahead of `prettify_host` purely for casing: it would render youtube.com as "Youtube".
    if let Some(h) = &host {
        if let Some(name) = platform_for_host(h) {
            return Some(name.to_string());
        }
    }
    // 6. Last resort: a prettified host (archived links resolve to nothing).
    host.as_deref().and_then(prettify_host)
}

/// Lowercased registrable host from a URL: scheme + `www.` stripped, path/query dropped.
/// `https://www.nytimes.com/2024/x` → `nytimes.com`; `https://blog.samaltman.com/y` → `blog.samaltman.com`.
fn host_of(url: &str) -> Option<String> {
    let s = url.trim();
    let s = s.split_once("://").map(|(_, rest)| rest).unwrap_or(s);
    let host = s.split(['/', '?', '#']).next()?.trim();
    let host = host.strip_prefix("www.").unwrap_or(host);
    (!host.is_empty()).then(|| host.to_lowercase())
}

/// Curated host → display name for the outlets and blogs the vault actually contains. Keys are the
/// `www.`-stripped host (full host for personal blogs/subdomains, registrable domain for outlets).
fn publisher_for_host(host: &str) -> Option<&'static str> {
    Some(match host {
        // Publications
        "nytimes.com" => "The New York Times",
        "wsj.com" => "WSJ",
        "newyorker.com" => "The New Yorker",
        "theatlantic.com" => "The Atlantic",
        "economist.com" => "The Economist",
        "wired.com" => "Wired",
        "forbes.com" => "Forbes",
        "nature.com" => "Nature",
        "cnbc.com" => "CNBC",
        "vox.com" => "Vox",
        // Company / org blogs
        "tesla.com" => "Tesla",
        "a16z.com" => "a16z",
        "menlovc.com" => "Menlo Ventures",
        "gatesnotes.com" => "Bill Gates",
        "blog.readwise.io" => "Readwise",
        // Personal blogs
        "blog.samaltman.com" | "samaltman.com" => "Sam Altman",
        "paulgraham.com" => "Paul Graham",
        "waitbutwhy.com" => "Wait But Why",
        "stephango.com" => "Steph Ango",
        "darioamodei.com" => "Dario Amodei",
        "aliabdaal.com" => "Ali Abdaal",
        "andrewchen.com" => "Andrew Chen",
        "madfientist.com" => "Mad Fientist",
        "erictopol.substack.com" => "Eric Topol",
        "noahpinion.blog" => "Noah Smith",
        "centuryofbio.com" => "Century of Bio",
        "worksinprogress.co" => "Works in Progress",
        "thezerotoone.co" => "Zero to One",
        "blueprint.bryanjohnson.com" => "Bryan Johnson",
        _ => return None,
    })
}

/// Hosts that name a **platform**, not a publication: the domain says where a post lives, never who
/// wrote it. Consulted only after `author:` and a person-looking `category` have both come up empty,
/// which is what keeps a real byline ("Jake Castillo") from being overwritten by its venue ("X").
///
/// Kept rather than simply deleted from the map, for two reasons: an author-less post should still
/// say something, and `prettify_host` gets the casing wrong — it renders `youtube.com` as "Youtube"
/// (though, by luck, `x.com` → "X").
///
/// `mobile.twitter.com` and `youtu.be` are here because [`host_of`] strips only `www.`, so without
/// them the same content scatters into extra buckets ("Twitter", "Youtu") in the author filter.
fn platform_for_host(host: &str) -> Option<&'static str> {
    Some(match host {
        "x.com" | "twitter.com" | "mobile.twitter.com" => "X",
        "youtube.com" | "m.youtube.com" | "youtu.be" => "YouTube",
        "medium.com" => "Medium",
        _ => return None,
    })
}

/// Is this `publisher:` value just naming a platform? The Web Clipper writes `publisher: Substack`
/// on a Substack post and `publisher: X` on a tweet, which would smuggle the venue back into the
/// byline through the front door — the very thing [`platform_for_host`] exists to stop. Falling
/// through instead lets `prettify_host` do better: `newhandle.substack.com` → "Newhandle".
fn is_platform_name(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "x" | "twitter" | "youtube" | "medium" | "substack"
    )
}

/// Heuristic: a `category` value that reads like a person's name — ≥2 words, each starting uppercase,
/// with at least one lowercase letter (so multi-word ALL-CAPS acronyms don't count). Catches
/// `Mark Cuban` / `Demis Hassabis`, rejects `AI` / `diet` / `boot strapping`. Deliberately fuzzy.
fn looks_like_person(cat: &str) -> bool {
    let words: Vec<&str> = cat.split_whitespace().collect();
    words.len() >= 2
        && words.iter().all(|w| w.chars().next().is_some_and(char::is_uppercase))
        && words.iter().any(|w| w.chars().skip(1).any(|c| c.is_lowercase()))
}

/// Rough name from an unmapped host: `*.substack.com` → the handle; otherwise the second-level
/// domain, title-cased. Archived-link hosts resolve to `None` (the real source is hidden).
fn prettify_host(host: &str) -> Option<String> {
    if matches!(host, "archive.ph" | "archive.today" | "archive.org" | "web.archive.org") {
        return None;
    }
    let labels: Vec<&str> = host.split('.').collect();
    let key = if host.ends_with(".substack.com") {
        labels.first().copied()
    } else {
        // Second-level label: `a.b.com` → `b`, `b.com` → `b`.
        labels.iter().rev().nth(1).copied()
    };
    key.map(titlecase).filter(|s| !s.is_empty())
}

fn titlecase(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(web: Option<&str>, author: Option<&str>, cats: &[&str]) -> Meta {
        Meta {
            web_link: web.map(Into::into),
            title: None,
            author: author.map(Into::into),
            publisher: None,
            published: None,
            created: None,
            status: None,
            categories: cats.iter().map(|s| s.to_string()).collect(),
            tags: vec![],
        }
    }
    fn byline(web: Option<&str>, author: Option<&str>, cats: &[&str]) -> Option<String> {
        derive_byline(&meta(web, author, cats))
    }
    fn byline_pub(web: Option<&str>, author: Option<&str>, publisher: Option<&str>) -> Option<String> {
        let mut m = meta(web, author, &[]);
        m.publisher = publisher.map(Into::into);
        derive_byline(&m)
    }

    #[test]
    fn known_publisher_wins_over_everything() {
        assert_eq!(byline(Some("https://www.nytimes.com/2024/x"), Some("Someone"), &["AI"]).as_deref(), Some("The New York Times"));
        // author empty, category a topic → still the mapped personal blog.
        assert_eq!(byline(Some("https://blog.samaltman.com/y"), Some(""), &["AI"]).as_deref(), Some("Sam Altman"));
        assert_eq!(byline(Some("https://wsj.com/a"), None, &[]).as_deref(), Some("WSJ"));
    }

    /// The regression this split exists for. Real frontmatter, captured verbatim from
    /// "23 Secrets of a $30M Influencer Marketing Program.md" in the vault on 2026-08-27: an x.com
    /// status URL plus a block-list wikilink author. It rendered as "X" — as did all 18 x.com and
    /// all 23 youtube.com clippings, every one of which names a real person.
    #[test]
    fn a_platform_never_outranks_a_named_author() {
        let jake = "https://x.com/jakecastilloooo/status/2074466709281309024?s=46";
        assert_eq!(byline(Some(jake), Some("Jake Castillo"), &[]).as_deref(), Some("Jake Castillo"));
        assert_eq!(
            byline(Some("https://www.youtube.com/watch?v=abc"), Some("Elon Musk"), &[]).as_deref(),
            Some("Elon Musk")
        );
        assert_eq!(
            byline(Some("https://medium.com/@someone/p"), Some("Some One"), &[]).as_deref(),
            Some("Some One")
        );
        // A person-looking category counts as "named" too, and still beats the platform.
        assert_eq!(
            byline(Some("https://www.youtube.com/watch?v=abc"), None, &["Mark Cuban"]).as_deref(),
            Some("Mark Cuban")
        );
    }

    /// With nobody named, the platform is still the best answer — and better than `prettify_host`,
    /// which would title-case `youtube.com` into "Youtube".
    #[test]
    fn a_platform_is_the_fallback_when_no_human_is_named() {
        assert_eq!(byline(Some("https://x.com/someone/status/1"), None, &[]).as_deref(), Some("X"));
        assert_eq!(byline(Some("https://x.com/someone/status/1"), Some(""), &["AI"]).as_deref(), Some("X"));
        assert_eq!(byline(Some("https://www.youtube.com/watch?v=a"), None, &[]).as_deref(), Some("YouTube"));
    }

    /// `host_of` strips only `www.`, so these used to scatter into separate author-filter buckets
    /// ("Twitter", "Youtu") for what is the same platform.
    #[test]
    fn platform_aliases_collapse_to_one_name() {
        for url in ["https://x.com/a/status/1", "https://twitter.com/a/status/1", "https://mobile.twitter.com/a/status/1"] {
            assert_eq!(byline(Some(url), None, &[]).as_deref(), Some("X"), "{url}");
        }
        for url in ["https://youtube.com/watch?v=a", "https://m.youtube.com/watch?v=a", "https://youtu.be/a"] {
            assert_eq!(byline(Some(url), None, &[]).as_deref(), Some("YouTube"), "{url}");
        }
    }

    /// `publisher:` fills the gap where nothing names a human. Hosts and values below are real,
    /// from the 21 vault clippings that carry a publisher but no author (2026-08-27).
    #[test]
    fn frontmatter_publisher_beats_a_guessed_domain() {
        // What these rendered as before: "Ft", "Umn", "Healthaffairs", "Wallstreetsurvivor".
        assert_eq!(byline_pub(Some("https://www.ft.com/content/x"), None, Some("Financial Times")).as_deref(), Some("Financial Times"));
        assert_eq!(byline_pub(Some("https://www.cidrap.umn.edu/x"), None, Some("CIDRAP")).as_deref(), Some("CIDRAP"));
        assert_eq!(byline_pub(Some("https://www.healthaffairs.org/x"), None, Some("Health Affairs")).as_deref(), Some("Health Affairs"));
        // ...and it outranks the platform too: this one rendered as "YouTube".
        assert_eq!(byline_pub(Some("https://m.youtube.com/watch?v=eCbwnwzJ8E8"), None, Some("Product Faculty")).as_deref(), Some("Product Faculty"));
    }

    /// A named human still wins. 167 vault clippings carry BOTH an author and a publisher; none of
    /// them may change, which is the whole point of putting this tier below the author.
    #[test]
    fn a_named_human_still_outranks_the_publisher() {
        assert_eq!(
            byline_pub(Some("https://www.bloomberg.com/x"), Some("Ashlee Vance"), Some("Bloomberg")).as_deref(),
            Some("Ashlee Vance")
        );
        // And a curated outlet still outranks both, unchanged.
        assert_eq!(
            byline_pub(Some("https://www.nytimes.com/x"), Some("Cade Metz"), Some("The New York Times")).as_deref(),
            Some("The New York Times")
        );
    }

    /// The clipper writes `publisher: Substack` / `publisher: X`, which would walk the venue back
    /// into the byline through a different door. Fall through and let the host do better.
    #[test]
    fn a_platform_named_in_publisher_is_ignored() {
        assert_eq!(byline_pub(Some("https://newhandle.substack.com/p"), None, Some("Substack")).as_deref(), Some("Newhandle"));
        assert_eq!(byline_pub(Some("https://x.com/a/status/1"), None, Some("X")).as_deref(), Some("X"));
        assert_eq!(byline_pub(Some("https://medium.com/@a/p"), None, Some("medium")).as_deref(), Some("Medium"));
    }

    #[test]
    fn author_when_domain_unknown() {
        assert_eq!(byline(Some("https://some-random-blog.net/p"), Some("Kenneth J. Arrow"), &[]).as_deref(), Some("Kenneth J. Arrow"));
    }

    #[test]
    fn person_category_when_no_author() {
        assert_eq!(byline(Some("https://an-unmapped-site.tld/p"), None, &["Mark Cuban"]).as_deref(), Some("Mark Cuban"));
        // Topic categories are skipped → falls through to the prettified host.
        assert_eq!(byline(Some("https://someblog.com/p"), None, &["AI", "diet"]).as_deref(), Some("Someblog"));
    }

    #[test]
    fn prettify_substack_and_archive_and_none() {
        assert_eq!(byline(Some("https://newhandle.substack.com/p"), None, &[]).as_deref(), Some("Newhandle"));
        assert_eq!(byline(Some("https://archive.ph/abcd"), None, &[]), None);
        assert_eq!(byline(None, None, &[]), None);
        assert_eq!(byline(None, Some(""), &["AI"]), None); // empty author + topic-only category
    }

    #[test]
    fn person_heuristic() {
        assert!(looks_like_person("Mark Cuban"));
        assert!(looks_like_person("Demis Hassabis"));
        assert!(!looks_like_person("AI"));
        assert!(!looks_like_person("VC"));
        assert!(!looks_like_person("boot strapping"));
        assert!(!looks_like_person("start ups"));
    }
}
