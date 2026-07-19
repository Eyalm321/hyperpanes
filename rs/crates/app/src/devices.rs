//! `hyperpanes devices` (list paired mobile clients) and `hyperpanes revoke <label>` (drop one).
//! Both drive the running control server's `/devices` endpoint as a local, master-authenticated
//! client — the same trust boundary as `hyperpanes pair`. See docs/mobile-client-plan.md.

use crate::control_cli;

pub fn wants_devices(argv: &[String]) -> bool {
    argv.get(1).map(|a| a == "devices").unwrap_or(false)
}

pub fn wants_revoke(argv: &[String]) -> bool {
    argv.get(1).map(|a| a == "revoke").unwrap_or(false)
}

/// `hyperpanes devices` — print each paired device's label + expiry (tokens are never shown).
pub fn run_list() -> std::io::Result<()> {
    let conn = control_cli::connect().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let resp = conn
        .client
        .get(format!("{}/devices", conn.base))
        .bearer_auth(&conn.token)
        .send()
        .map_err(|e| std::io::Error::other(format!("GET {}/devices: {e}", conn.base)))?;
    if !resp.status().is_success() {
        eprintln!(
            "server returned {}: {}",
            resp.status(),
            resp.text().unwrap_or_default()
        );
        std::process::exit(1);
    }
    let v: serde_json::Value = resp
        .json()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let devices = v
        .get("devices")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    if devices.is_empty() {
        println!("No paired devices. Run `hyperpanes pair` to add one.");
        return Ok(());
    }
    println!("Paired devices:");
    for d in &devices {
        let label = d.get("label").and_then(|l| l.as_str()).unwrap_or("?");
        let expiry = match d.get("expiresAt").and_then(|e| e.as_i64()) {
            Some(ms) => format!("expires at {ms} (ms epoch)"),
            None => "never expires".to_string(),
        };
        println!("  {label}  —  {expiry}");
    }
    Ok(())
}

/// `hyperpanes revoke <label>` — revoke every device carrying that label.
pub fn run_revoke(argv: &[String]) -> std::io::Result<()> {
    let Some(label) = argv.get(2).filter(|s| !s.is_empty()) else {
        eprintln!("usage: hyperpanes revoke <label>   (see `hyperpanes devices` for labels)");
        std::process::exit(2);
    };
    let conn = control_cli::connect().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let resp = conn
        .client
        .delete(format!("{}/devices", conn.base))
        .query(&[("label", label.as_str())])
        .bearer_auth(&conn.token)
        .send()
        .map_err(|e| std::io::Error::other(format!("DELETE {}/devices: {e}", conn.base)))?;
    if !resp.status().is_success() {
        eprintln!(
            "server returned {}: {}",
            resp.status(),
            resp.text().unwrap_or_default()
        );
        std::process::exit(1);
    }
    let v: serde_json::Value = resp
        .json()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let revoked = v.get("revoked").and_then(|r| r.as_i64()).unwrap_or(0);
    if revoked > 0 {
        println!("Revoked {revoked} device(s) labelled \"{label}\".");
    } else {
        println!("No device labelled \"{label}\" (nothing revoked).");
    }
    Ok(())
}
