//! "Needs input" desktop alerts. A pane whose screen ends in a decision dialog (a permission or
//! trust dialog, a picker) is in an ask episode and its tab reads ASK (core
//! `session::activity::AskEpisode`). Once an episode has lasted [`ASK_ALERT_AFTER_MS`], the owner
//! gets ONE desktop notification for it: "<tab> needs input", "<pane label> is waiting on a
//! permission prompt". The body names only the kind of prompt (`AskKind`), never text from the
//! screen: a notification can show on the lock screen and stays in the notification history,
//! and a screen row can carry a command line, a token or a path. Episodes that come due in the
//! same poll share one notification. A new episode alerts again; the setting `askNotify` turns
//! alerts off.
//!
//! [`AskAlerts`] is the pure once-per-episode policy (tested with a fake clock); a [`Notifier`]
//! delivers: [`DesktopNotifier`] over `org.freedesktop.Notifications` on Linux, nothing elsewhere.

use std::collections::HashMap;

use hyperpanes_core::session::activity::AskKind;

/// How long a pane must have been asking, and this app watching it ask, before its episode
/// raises an alert.
pub const ASK_ALERT_AFTER_MS: u64 = 60_000;

/// The most panes one grouped notification lists by name; the rest are counted.
pub const GROUP_LIST_MAX: usize = 4;

/// A pane currently in an ask episode, with what an alert shows about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskingPane {
    pub uid: String,
    pub tab: String,
    pub label: String,
    /// When the episode began (epoch ms): the episode's identity.
    pub since: u64,
    pub kind: AskKind,
}

/// One desktop notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub title: String,
    pub body: String,
}

/// Delivers alerts. Behind a trait so tests never raise a real notification.
pub trait Notifier {
    fn notify(&self, alert: &Alert);
}

/// The kind of prompt, as an alert names it.
pub fn kind_name(kind: AskKind) -> &'static str {
    match kind {
        AskKind::Permission => "permission prompt",
        AskKind::Trust => "folder trust prompt",
        AskKind::Choice => "choice",
    }
}

/// The one notification for the panes that came due together (none for an empty list).
pub fn alert_for(due: &[&AskingPane]) -> Option<Alert> {
    match due {
        [] => None,
        [p] => Some(Alert {
            title: format!("{} needs input", p.tab),
            body: format!("{} is waiting on a {}", p.label, kind_name(p.kind)),
        }),
        _ => {
            let mut lines: Vec<String> = due
                .iter()
                .take(GROUP_LIST_MAX)
                .map(|p| format!("{}: {}, {}", p.tab, p.label, kind_name(p.kind)))
                .collect();
            if due.len() > GROUP_LIST_MAX {
                lines.push(format!("and {} more", due.len() - GROUP_LIST_MAX));
            }
            Some(Alert {
                title: format!("{} panes need input", due.len()),
                body: lines.join("\n"),
            })
        }
    }
}

/// What the policy remembers about one pane's episode.
#[derive(Debug, Clone, Copy)]
struct Seen {
    /// The episode's `since`.
    since: u64,
    /// When this app first saw the episode (epoch ms).
    first_seen: u64,
    alerted: bool,
}

/// The once-per-episode policy. Remembers, per pane, the episode it last saw and whether it
/// alerted for it.
#[derive(Debug, Default)]
pub struct AskAlerts {
    seen: HashMap<String, Seen>,
}

impl AskAlerts {
    /// The notification due at `now` for the panes currently `asking`, if any: one covering
    /// every episode that has lasted [`ASK_ALERT_AFTER_MS`], has been watched by this app that
    /// long too, and was not alerted yet. The watch rule matters after a restart: an episode that
    /// began before it (its `since` comes from the pane's last output) alerts a minute after the
    /// app sees it, not at once. Episodes that ended, or whose pane is left out (its window has
    /// alerts off), are forgotten, so the next one, or the same one once alerts are back on,
    /// alerts a minute after it is seen again.
    pub fn poll(&mut self, asking: &[AskingPane], now: u64) -> Option<Alert> {
        self.seen
            .retain(|uid, s| asking.iter().any(|p| &p.uid == uid && p.since == s.since));
        for p in asking {
            self.seen.entry(p.uid.clone()).or_insert(Seen {
                since: p.since,
                first_seen: now,
                alerted: false,
            });
        }
        let mut due = Vec::new();
        for p in asking {
            let Some(s) = self.seen.get_mut(&p.uid) else {
                continue;
            };
            if !s.alerted && now.saturating_sub(s.since.max(s.first_seen)) >= ASK_ALERT_AFTER_MS {
                s.alerted = true;
                due.push(p);
            }
        }
        alert_for(&due)
    }
}

/// How long one notification may take before it is given up on (`crate::dbus::notify`).
pub const NOTIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The app's notifier: a desktop notification on Linux (skipped when the session bus is known
/// to be unusable), nothing elsewhere. One worker thread, started on the first alert, sends them
/// in order off the UI thread, each given up on after [`NOTIFY_TIMEOUT`]: a slow or hung
/// notification daemon delays alerts but never stalls the UI or piles up threads.
#[derive(Default)]
pub struct DesktopNotifier {
    #[cfg(target_os = "linux")]
    queue: std::cell::OnceCell<Option<std::sync::mpsc::Sender<Alert>>>,
}

impl Notifier for DesktopNotifier {
    #[cfg(target_os = "linux")]
    fn notify(&self, alert: &Alert) {
        if crate::dbus::session_bus_unavailable() {
            return;
        }
        let queue = self.queue.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel::<Alert>();
            let spawned = std::thread::Builder::new()
                .name("hp-ask-notify".into())
                .spawn(move || {
                    for alert in rx {
                        if let Err(e) = crate::dbus::notify(
                            "Hyperpanes",
                            &alert.title,
                            &alert.body,
                            NOTIFY_TIMEOUT,
                        ) {
                            crate::dbg_log(&format!("ask alert: notification failed: {e}"));
                        }
                    }
                });
            match spawned {
                Ok(_) => Some(tx),
                Err(e) => {
                    crate::dbg_log(&format!(
                        "ask alert: could not start the notify thread: {e}"
                    ));
                    None
                }
            }
        });
        if let Some(tx) = queue {
            let _ = tx.send(alert.clone());
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn notify(&self, _alert: &Alert) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeNotifier(RefCell<Vec<Alert>>);
    impl Notifier for FakeNotifier {
        fn notify(&self, alert: &Alert) {
            self.0.borrow_mut().push(alert.clone());
        }
    }

    fn pane(uid: &str, since: u64) -> AskingPane {
        AskingPane {
            uid: uid.into(),
            tab: "security".into(),
            label: "reviewer".into(),
            since,
            kind: AskKind::Permission,
        }
    }

    // Drive the policy into a notifier the way the app does.
    fn tick(alerts: &mut AskAlerts, n: &dyn Notifier, asking: &[AskingPane], now: u64) {
        if let Some(a) = alerts.poll(asking, now) {
            n.notify(&a);
        }
    }

    #[test]
    fn alerts_once_per_episode_after_a_minute() {
        let n = FakeNotifier::default();
        let mut alerts = AskAlerts::default();
        let asking = [pane("u1", 1_000)];
        for now in [1_000, 1_000 + ASK_ALERT_AFTER_MS - 1] {
            tick(&mut alerts, &n, &asking, now);
        }
        assert!(n.0.borrow().is_empty());
        for s in 0..10 {
            tick(
                &mut alerts,
                &n,
                &asking,
                1_000 + ASK_ALERT_AFTER_MS + s * 1_000,
            );
        }
        assert_eq!(
            *n.0.borrow(),
            vec![Alert {
                title: "security needs input".into(),
                body: "reviewer is waiting on a permission prompt".into(),
            }]
        );
    }

    #[test]
    fn the_body_names_the_prompt_kind_never_screen_text() {
        let mut p = pane("u1", 0);
        for (kind, body) in [
            (
                AskKind::Permission,
                "reviewer is waiting on a permission prompt",
            ),
            (
                AskKind::Trust,
                "reviewer is waiting on a folder trust prompt",
            ),
            (AskKind::Choice, "reviewer is waiting on a choice"),
        ] {
            p.kind = kind;
            assert_eq!(alert_for(&[&p]).unwrap().body, body);
        }
    }

    #[test]
    fn a_new_episode_alerts_again() {
        let n = FakeNotifier::default();
        let mut alerts = AskAlerts::default();
        tick(&mut alerts, &n, &[pane("u1", 0)], 0);
        tick(&mut alerts, &n, &[pane("u1", 0)], ASK_ALERT_AFTER_MS);
        // The episode ends (the pane stopped asking)…
        tick(&mut alerts, &n, &[], ASK_ALERT_AFTER_MS + 1_000);
        // …and a later one starts and lasts a minute.
        let later = ASK_ALERT_AFTER_MS + 5_000;
        tick(&mut alerts, &n, &[pane("u1", later)], later + 1_000);
        assert_eq!(n.0.borrow().len(), 1);
        tick(
            &mut alerts,
            &n,
            &[pane("u1", later)],
            later + 1_000 + ASK_ALERT_AFTER_MS,
        );
        assert_eq!(n.0.borrow().len(), 2);
    }

    #[test]
    fn panes_due_together_share_one_notification() {
        let mut alerts = AskAlerts::default();
        let mut panes: Vec<AskingPane> = (0..6).map(|i| pane(&format!("u{i}"), 0)).collect();
        panes[1].tab = "build".into();
        panes[1].label = "agent".into();
        panes[1].kind = AskKind::Choice;
        assert_eq!(alerts.poll(&panes, 0), None);
        let a = alerts.poll(&panes, ASK_ALERT_AFTER_MS).unwrap();
        assert_eq!(a.title, "6 panes need input");
        let lines: Vec<&str> = a.body.lines().collect();
        assert_eq!(lines.len(), GROUP_LIST_MAX + 1);
        assert_eq!(lines[0], "security: reviewer, permission prompt");
        assert_eq!(lines[1], "build: agent, choice");
        assert_eq!(lines[GROUP_LIST_MAX], "and 2 more");
        // All six were alerted: nothing more for these episodes.
        assert_eq!(alerts.poll(&panes, ASK_ALERT_AFTER_MS + 1_000), None);
    }

    // After a restart, an episode's `since` comes from the pane's last output, possibly hours
    // ago: it alerts a minute after the app first sees it, not on the first poll.
    #[test]
    fn an_old_episode_first_seen_after_a_restart_waits_a_minute() {
        let mut alerts = AskAlerts::default();
        let start = 10 * 3_600_000;
        let asking = [pane("u1", start - 3 * 3_600_000)];
        assert_eq!(alerts.poll(&asking, start), None);
        assert_eq!(alerts.poll(&asking, start + ASK_ALERT_AFTER_MS - 1), None);
        assert!(alerts.poll(&asking, start + ASK_ALERT_AFTER_MS).is_some());
    }

    // A window with alerts off leaves its panes out of the poll.
    #[test]
    fn a_pane_left_out_while_alerts_are_off_alerts_a_minute_after_they_are_back_on() {
        let n = FakeNotifier::default();
        let mut alerts = AskAlerts::default();
        let asking = [pane("u1", 0)];
        tick(&mut alerts, &n, &asking, 0);
        for s in 0..5 {
            tick(&mut alerts, &n, &[], ASK_ALERT_AFTER_MS + s * 1_000);
        }
        assert!(n.0.borrow().is_empty());
        // Back on while the episode still runs: it alerts once, a minute later.
        let on = 2 * ASK_ALERT_AFTER_MS;
        tick(&mut alerts, &n, &asking, on);
        assert!(n.0.borrow().is_empty());
        tick(&mut alerts, &n, &asking, on + ASK_ALERT_AFTER_MS);
        tick(&mut alerts, &n, &asking, on + ASK_ALERT_AFTER_MS + 1_000);
        assert_eq!(n.0.borrow().len(), 1);
    }
}
