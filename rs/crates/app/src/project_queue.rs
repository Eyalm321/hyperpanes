//! Queue → project mapping and per-project task-count aggregation (hyperpanes#6).
//!
//! A work queue belongs to a registered project when, for one of the project's KEYS (its `id`
//! or its `name`, compared case-insensitively), the queue name equals the key or starts with
//! `<key>:` or `<key>-` (goal queues look like `<project>-g1` or `<project>-1a2b3c-t3`). When
//! several projects match, the LONGEST key wins (so `hyper` never takes `hyperpanes-…`); an
//! equal-length tie between two different projects is ambiguous. Unmatched and ambiguous queues
//! go to the "Other queues" bucket and are never dropped. See `docs/queue-gui-roadmap.md`
//! ("How queues map to projects"). Everything here is pure so the GUI only renders the result.

use std::collections::HashMap;

use hyperpanes_core::control::work::QueueSummary;

use crate::sidebar::Project;

/// Whether `queue` is addressed by `key`: equal, or `<key>:…` / `<key>-…`, ignoring ASCII case.
fn key_matches(queue: &str, key: &str) -> bool {
    // `is_char_boundary` is false past the end, so a shorter queue (or a split inside a
    // multi-byte char) never reaches `split_at`.
    if key.is_empty() || !queue.is_char_boundary(key.len()) {
        return false;
    }
    let (head, rest) = queue.split_at(key.len());
    head.eq_ignore_ascii_case(key)
        && (rest.is_empty() || rest.starts_with(':') || rest.starts_with('-'))
}

/// The id of the project `queue` belongs to, or `None` for the "Other queues" bucket (no
/// project matches, or the longest match is an equal-length tie between two projects).
pub fn project_for_queue<'a>(queue: &str, projects: &'a [Project]) -> Option<&'a str> {
    let mut best: Option<(usize, &'a str)> = None;
    let mut tied = false;
    for p in projects {
        for key in [p.id.as_str(), p.name.as_str()] {
            if !key_matches(queue, key) {
                continue;
            }
            match best {
                Some((len, id)) if key.len() < len || (key.len() == len && id == p.id) => {}
                Some((len, _)) if key.len() == len => tied = true,
                _ => {
                    best = Some((key.len(), p.id.as_str()));
                    tied = false;
                }
            }
        }
    }
    if tied {
        None
    } else {
        best.map(|(_, id)| id)
    }
}

/// Non-terminal task count (queued + claimed) of the queues that map to NO project (the
/// "Other queues" bucket). Never dropped: the sidebar shows it as its own row.
pub fn other_open_count(queues: &[QueueSummary], projects: &[Project]) -> usize {
    queues
        .iter()
        .filter(|q| project_for_queue(&q.queue, projects).is_none())
        .map(|q| q.counts.queued + q.counts.claimed)
        .sum()
}

/// Non-terminal task count (queued + claimed) per project id, summed over every mapped
/// queue. Projects with a zero count are omitted.
pub fn project_open_counts(
    queues: &[QueueSummary],
    projects: &[Project],
) -> HashMap<String, usize> {
    let mut out: HashMap<String, usize> = HashMap::new();
    for q in queues {
        if let Some(id) = project_for_queue(&q.queue, projects) {
            let n = q.counts.queued + q.counts.claimed;
            if n > 0 {
                *out.entry(id.to_string()).or_default() += n;
            }
        }
    }
    out
}

/// Badge text for a count: nothing at 0, else the number.
pub fn badge_text(n: usize) -> String {
    if n == 0 {
        String::new()
    } else {
        n.to_string()
    }
}

/// The default worker queue for a working directory: the id of the registered project
/// whose repo root contains `cwd`. `git_root` is the cwd's git root (if any).
pub fn default_queue_for(git_root: Option<&str>, projects: &[Project]) -> Option<String> {
    let root = git_root?;
    projects
        .iter()
        .find(|p| p.path.trim_end_matches('/') == root.trim_end_matches('/'))
        .map(|p| p.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperpanes_core::control::work::Counts;

    fn proj(id: &str, path: &str) -> Project {
        named(id, id, path)
    }

    fn named(id: &str, name: &str, path: &str) -> Project {
        Project {
            id: id.into(),
            path: path.into(),
            name: name.into(),
            color: "#fff".into(),
            last_opened_at: None,
        }
    }

    /// The real-world table: registry ids are UUIDs, goal queues are prefixed with the NAME.
    #[test]
    fn queue_to_project_table() {
        let ps = vec![
            named("11111111-uuid", "hyperpanes", "/d/hyperpanes"),
            named("22222222-uuid", "hyperpanes-mcp", "/d/hyperpanes-mcp"),
            named("aaaa-uuid", "hyper", "/d/hyper"),
            named("bbbb-uuid", "WebApp", "/d/WebApp"),
            named("cccc-uuid", "myapp", "/d/myapp"),
            named("dddd-uuid", "twin", "/d/a/twin"),
            named("eeee-uuid", "twin", "/d/b/twin"),
        ];
        let cases: &[(&str, Option<&str>)] = &[
            // name prefix with `-`: goal queues
            ("hyperpanes-1a2b3c-t3", Some("11111111-uuid")),
            ("myapp-g1", Some("cccc-uuid")),
            // longest key wins: hyperpanes-mcp, not hyperpanes; hyperpanes, not hyper
            ("hyperpanes-mcp-g1", Some("22222222-uuid")),
            ("hyperpanes", Some("11111111-uuid")),
            ("hyper-x", Some("aaaa-uuid")),
            // id exact / id with `:` / id with `-`
            ("11111111-uuid", Some("11111111-uuid")),
            ("11111111-uuid:g1", Some("11111111-uuid")),
            ("cccc-uuid-g9", Some("cccc-uuid")),
            // case-insensitive name
            ("webapp-g2", Some("bbbb-uuid")),
            // a prefix that runs into letters is not a match
            ("hyperpanesx", None),
            ("myappg1", None),
            // two registered repos with the same name: ambiguous, never guessed
            ("twin-g1", None),
            // unrelated / legacy bare names go to Other
            ("g12", None),
            ("tools", None),
            ("", None),
            ("hyp", None),
            ("hyperpanés-g1", None),
        ];
        for (q, want) in cases {
            assert_eq!(project_for_queue(q, &ps), *want, "queue {q:?}");
        }
    }

    #[test]
    fn unmatched_and_ambiguous_queues_count_as_other() {
        let ps = vec![
            named("p1", "alpha", "/r/a"),
            named("p2", "twin", "/r/t1"),
            named("p3", "twin", "/r/t2"),
        ];
        let q = vec![
            qs("alpha-g1", 2, 1, 0),
            qs("g1", 1, 1, 4),
            qs("twin-g2", 3, 0, 0),
            qs("zzz", 0, 0, 7),
        ];
        assert_eq!(other_open_count(&q, &ps), 5);
        assert_eq!(project_open_counts(&q, &ps).get("p1"), Some(&3));
    }

    fn qs(queue: &str, queued: usize, claimed: usize, done: usize) -> QueueSummary {
        QueueSummary {
            queue: queue.into(),
            counts: Counts {
                queued,
                claimed,
                done,
                ..Counts::default()
            },
        }
    }

    #[test]
    fn maps_exact_id_and_subqueue_not_lookalikes() {
        let ps = vec![proj("abc", "/r/a"), proj("abcd", "/r/b")];
        assert_eq!(project_for_queue("abc", &ps), Some("abc"));
        assert_eq!(project_for_queue("abc:g1", &ps), Some("abc"));
        assert_eq!(project_for_queue("abcd", &ps), Some("abcd"));
        assert_eq!(project_for_queue("abcd:x", &ps), Some("abcd"));
        assert_eq!(project_for_queue("abcx", &ps), None);
        assert_eq!(project_for_queue("other", &ps), None);
        assert_eq!(project_for_queue("", &ps), None);
    }

    #[test]
    fn aggregates_non_terminal_across_queues_for_two_projects() {
        let ps = vec![proj("a", "/r/a"), proj("b", "/r/b")];
        let q = vec![
            qs("a", 2, 1, 5),
            qs("a:g1", 1, 0, 0),
            qs("b", 0, 0, 9),
            qs("zzz", 7, 7, 7),
        ];
        let m = project_open_counts(&q, &ps);
        assert_eq!(m.get("a"), Some(&4));
        assert_eq!(m.get("b"), None);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn badge_text_hides_zero() {
        assert_eq!(badge_text(0), "");
        assert_eq!(badge_text(3), "3");
    }

    #[test]
    fn default_queue_matches_project_root() {
        let ps = vec![proj("p1", "/r/a"), proj("p2", "/r/b/")];
        assert_eq!(default_queue_for(Some("/r/a"), &ps), Some("p1".into()));
        assert_eq!(default_queue_for(Some("/r/b"), &ps), Some("p2".into()));
        assert_eq!(default_queue_for(Some("/r/c"), &ps), None);
        assert_eq!(default_queue_for(None, &ps), None);
    }
}
