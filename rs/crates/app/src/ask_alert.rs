//! "Needs input" desktop alerts. A pane whose screen waits on a decision prompt (a permission or
//! confirm dialog) is in an ask episode and its tab reads ASK (core `session::activity::AskEpisode`).
//! Once an episode has lasted [`ASK_ALERT_AFTER_MS`], the owner gets ONE desktop notification for
//! it, titled "<tab> needs input" with "<pane label>: <question>" as the body. A new episode
//! alerts again; the setting `askNotify` turns alerts off.
//!
//! [`AskAlerts`] is the pure once-per-episode policy (tested with a fake clock); a [`Notifier`]
//! delivers: [`DesktopNotifier`] over `org.freedesktop.Notifications` on Linux, nothing elsewhere.

use std::collections::HashMap;

/// How long a pane must have been asking before its episode raises an alert.
pub const ASK_ALERT_AFTER_MS: u64 = 60_000;

/// A pane currently in an ask episode, with what an alert shows about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskingPane {
    pub uid: String,
    pub tab: String,
    pub label: String,
    /// When the episode began (epoch ms): the episode's identity.
    pub since: u64,
    pub question: Option<String>,
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

/// The alert for `pane`.
pub fn alert_for(pane: &AskingPane) -> Alert {
    let body = match &pane.question {
        Some(q) => format!("{}: {q}", pane.label),
        None => format!("{}: waiting on a confirmation", pane.label),
    };
    Alert {
        title: format!("{} needs input", pane.tab),
        body,
    }
}

/// The once-per-episode policy. Remembers, per pane, the episode it already alerted for.
#[derive(Debug, Default)]
pub struct AskAlerts {
    /// uid → `since` of the episode already alerted.
    alerted: HashMap<String, u64>,
}

impl AskAlerts {
    /// The alerts due at `now` for the panes currently `asking`: one per episode that has lasted
    /// [`ASK_ALERT_AFTER_MS`] and was not alerted yet. Episodes that ended are forgotten, so the
    /// next one alerts again. With `enabled` off nothing is due (an episode still running when
    /// alerts are switched back on alerts then).
    pub fn poll(&mut self, asking: &[AskingPane], now: u64, enabled: bool) -> Vec<Alert> {
        self.alerted
            .retain(|uid, since| asking.iter().any(|p| &p.uid == uid && p.since == *since));
        if !enabled {
            return Vec::new();
        }
        let mut due = Vec::new();
        for p in asking {
            if now.saturating_sub(p.since) >= ASK_ALERT_AFTER_MS
                && !self.alerted.contains_key(&p.uid)
            {
                self.alerted.insert(p.uid.clone(), p.since);
                due.push(alert_for(p));
            }
        }
        due
    }
}

/// The app's notifier: a desktop notification on Linux (skipped when the session bus is known
/// to be unusable), nothing elsewhere. Each alert is sent from its own short-lived thread, so a
/// slow bus never stalls the UI.
pub struct DesktopNotifier;

impl Notifier for DesktopNotifier {
    #[cfg(target_os = "linux")]
    fn notify(&self, alert: &Alert) {
        if crate::dbus::session_bus_unavailable() {
            return;
        }
        let alert = alert.clone();
        let spawned = std::thread::Builder::new()
            .name("hp-ask-notify".into())
            .spawn(move || {
                if let Err(e) = crate::dbus::notify("Hyperpanes", &alert.title, &alert.body) {
                    crate::dbg_log(&format!("ask alert: notification failed: {e}"));
                }
            });
        if let Err(e) = spawned {
            crate::dbg_log(&format!(
                "ask alert: could not start the notify thread: {e}"
            ));
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
            question: Some("Do you want to proceed?".into()),
        }
    }

    // Drive the policy into a notifier the way the app does.
    fn tick(alerts: &mut AskAlerts, n: &dyn Notifier, asking: &[AskingPane], now: u64, on: bool) {
        for a in alerts.poll(asking, now, on) {
            n.notify(&a);
        }
    }

    #[test]
    fn alerts_once_per_episode_after_a_minute() {
        let n = FakeNotifier::default();
        let mut alerts = AskAlerts::default();
        let asking = [pane("u1", 1_000)];
        tick(
            &mut alerts,
            &n,
            &asking,
            1_000 + ASK_ALERT_AFTER_MS - 1,
            true,
        );
        assert!(n.0.borrow().is_empty());
        for s in 0..10 {
            tick(
                &mut alerts,
                &n,
                &asking,
                1_000 + ASK_ALERT_AFTER_MS + s * 1_000,
                true,
            );
        }
        assert_eq!(
            *n.0.borrow(),
            vec![Alert {
                title: "security needs input".into(),
                body: "reviewer: Do you want to proceed?".into(),
            }]
        );
    }

    #[test]
    fn a_new_episode_alerts_again() {
        let n = FakeNotifier::default();
        let mut alerts = AskAlerts::default();
        tick(&mut alerts, &n, &[pane("u1", 0)], ASK_ALERT_AFTER_MS, true);
        // The episode ends (the pane stopped asking)…
        tick(&mut alerts, &n, &[], ASK_ALERT_AFTER_MS + 1_000, true);
        // …and a later one starts and lasts a minute.
        let later = ASK_ALERT_AFTER_MS + 5_000;
        tick(&mut alerts, &n, &[pane("u1", later)], later + 1_000, true);
        assert_eq!(n.0.borrow().len(), 1);
        tick(
            &mut alerts,
            &n,
            &[pane("u1", later)],
            later + ASK_ALERT_AFTER_MS,
            true,
        );
        assert_eq!(n.0.borrow().len(), 2);
    }

    #[test]
    fn panes_alert_independently_and_name_their_question() {
        let mut alerts = AskAlerts::default();
        let mut b = pane("u2", 10_000);
        b.tab = "build".into();
        b.label = "agent".into();
        b.question = None;
        let due = alerts.poll(&[pane("u1", 0), b], 10_000 + ASK_ALERT_AFTER_MS, true);
        assert_eq!(due.len(), 2);
        assert_eq!(due[1].title, "build needs input");
        assert_eq!(due[1].body, "agent: waiting on a confirmation");
    }

    #[test]
    fn the_setting_off_sends_nothing() {
        let n = FakeNotifier::default();
        let mut alerts = AskAlerts::default();
        let asking = [pane("u1", 0)];
        for s in 0..5 {
            tick(
                &mut alerts,
                &n,
                &asking,
                ASK_ALERT_AFTER_MS + s * 1_000,
                false,
            );
        }
        assert!(n.0.borrow().is_empty());
        // Switched back on while the episode still runs: it alerts once.
        tick(&mut alerts, &n, &asking, ASK_ALERT_AFTER_MS + 9_000, true);
        tick(&mut alerts, &n, &asking, ASK_ALERT_AFTER_MS + 10_000, true);
        assert_eq!(n.0.borrow().len(), 1);
    }
}
