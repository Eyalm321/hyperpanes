//! Queue → project mapping and per-project task-count aggregation (hyperpanes#6).
//!
//! A work queue belongs to a registered project when its name EQUALS the project's `id`
//! or starts with `<project id>:` (a sub-queue such as `<id>:g1`). Queues matching no
//! project are not shown. See `docs/queue-gui-roadmap.md` ("How queues map to projects").
//! Everything here is pure so the GUI only renders the result.

use std::collections::HashMap;

use hyperpanes_core::control::work::QueueSummary;

use crate::sidebar::Project;

/// The id of the project `queue` belongs to, if any. When several project ids match
/// (one id being a `:`-prefix of another), the longest id wins.
pub fn project_for_queue<'a>(queue: &str, projects: &'a [Project]) -> Option<&'a str> {
    projects
        .iter()
        .filter(|p| {
            queue == p.id
                || queue
                    .strip_prefix(p.id.as_str())
                    .is_some_and(|rest| rest.starts_with(':'))
        })
        .max_by_key(|p| p.id.len())
        .map(|p| p.id.as_str())
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
        Project {
            id: id.into(),
            path: path.into(),
            name: id.into(),
            color: "#fff".into(),
            last_opened_at: None,
        }
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
