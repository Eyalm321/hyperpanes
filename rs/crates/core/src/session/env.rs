//! Fresh per-spawn environment (#28).
//!
//! `std::env::vars()` is the APP process's environment, frozen at app launch — an
//! installer that appended a PATH entry, or an auth tool that set a user variable,
//! after the app started would never reach a NEW pane. Windows publishes the durable
//! environment in the registry (`HKLM\...\Session Manager\Environment` for the
//! machine, `HKCU\Environment` for the user), which is what a freshly launched
//! console would see — so [`fresh_env`] rebuilds the spawn base from there on every
//! spawn:
//!
//! 1. machine vars, then user vars layered on top (user wins per-var, EXCEPT `PATH`,
//!    which is machine PATH + `;` + user PATH — the OS logon rule),
//! 2. `REG_EXPAND_SZ` values expanded (`%SystemRoot%` etc.) against the merged map,
//!    falling back to the process env for tokens the registry doesn't define,
//! 3. process-env vars that are NOT registry-backed layered in (session-only vars:
//!    `HYPERPANES_*` injections, tokens handed to us by a parent, etc.) — for a name
//!    present in both, the FRESH registry value wins over the stale process copy.
//!
//! Per-pane `opts.env` overrides and the `HYPERPANES_PANE_ID`/control-file injection
//! still happen afterwards in [`super::spawn::build_env`], unchanged.
//!
//! The registry read is `#[cfg(windows)]` (elsewhere the process env IS the freshest
//! source); the merge itself is pure and unit-tested with injected maps.

use super::spawn::EnvMap;

/// One raw registry environment value: `(name, raw data, is REG_EXPAND_SZ)`.
pub type RawVar = (String, String, bool);

/// The freshest spawn-base environment this platform can produce. Windows: the
/// registry-merged machine+user environment with process-only vars layered in (see
/// the module docs). Non-Windows, or if the registry is unreadable: the process env.
pub fn fresh_env() -> EnvMap {
    let process: EnvMap = std::env::vars().collect();
    fresh_env_with_process(process)
}

#[cfg(windows)]
fn fresh_env_with_process(process: EnvMap) -> EnvMap {
    let machine = registry::read_env_key(
        registry::HKEY_LOCAL_MACHINE,
        r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
    );
    let user = registry::read_env_key(registry::HKEY_CURRENT_USER, "Environment");
    if machine.is_none() && user.is_none() {
        return process; // registry unreadable — the frozen env beats no env
    }
    merge_fresh_env(
        &machine.unwrap_or_default(),
        &user.unwrap_or_default(),
        &process,
    )
}

#[cfg(not(windows))]
fn fresh_env_with_process(process: EnvMap) -> EnvMap {
    process
}

/// Pure merge core of [`fresh_env`] — see the module docs for the three layers.
/// Registry names are matched case-insensitively (Windows env semantics); the
/// returned map keeps each winner's original spelling.
pub fn merge_fresh_env(machine: &[RawVar], user: &[RawVar], process: &EnvMap) -> EnvMap {
    // 1. machine ◁ user (CI upsert; PATH concatenates instead of replacing).
    let mut merged: Vec<RawVar> = machine.to_vec();
    for (name, value, expand) in user {
        if name.eq_ignore_ascii_case("PATH") {
            if let Some(slot) = merged.iter_mut().find(|(n, _, _)| n.eq_ignore_ascii_case("PATH")) {
                let mp = slot.1.trim_end_matches(';');
                slot.1 = if mp.is_empty() { value.clone() } else { format!("{mp};{value}") };
                slot.2 = slot.2 || *expand;
                continue;
            }
        }
        match merged.iter_mut().find(|(n, _, _)| n.eq_ignore_ascii_case(name)) {
            Some(slot) => {
                slot.1 = value.clone();
                slot.2 = *expand;
            }
            None => merged.push((name.clone(), value.clone(), *expand)),
        }
    }

    // 2. expand REG_EXPAND_SZ values: %TOKEN% resolves against the merged registry
    //    map first, then the process env; an unknown token stays literal (OS parity).
    let lookup = |name: &str| -> Option<String> {
        merged
            .iter()
            .find(|(n, _, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v, _)| v.clone())
            .or_else(|| {
                process
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v.clone())
            })
    };
    let mut env = EnvMap::new();
    for (name, value, expand) in &merged {
        let v = if *expand { expand_value(value, &lookup) } else { value.clone() };
        env.insert(name.clone(), v);
    }

    // 3. layer process-only vars (session vars, HYPERPANES_* injections). A name the
    //    registry also defines keeps the FRESH registry value.
    for (k, v) in process {
        if !env.keys().any(|n| n.eq_ignore_ascii_case(k)) {
            env.insert(k.clone(), v.clone());
        }
    }
    env
}

/// Expand `%NAME%` tokens in a `REG_EXPAND_SZ` value. Unknown tokens are left
/// verbatim and an unpaired `%` passes through, matching `ExpandEnvironmentStrings`.
fn expand_value(value: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match lookup(name) {
                    Some(v) if !name.is_empty() => out.push_str(&v),
                    _ => {
                        // unknown (or empty) token: keep it literal, including both %s
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Minimal registry-read FFI (advapi32 via `#[link]` — the workspace's `windows`
/// crate doesn't enable `Win32_System_Registry`, and `Cargo.toml` is scaffold-frozen
/// for parallel tracks). Read-only: open key → enumerate string values → close.
#[cfg(windows)]
mod registry {
    use super::RawVar;
    use std::ptr;

    type Hkey = isize;
    // Predefined roots are sign-extended on 64-bit (`(HKEY)(LONG)0x8000000x`).
    pub const HKEY_CURRENT_USER: Hkey = 0x8000_0001u32 as i32 as isize;
    pub const HKEY_LOCAL_MACHINE: Hkey = 0x8000_0002u32 as i32 as isize;
    const KEY_READ: u32 = 0x2_0019;
    const ERROR_SUCCESS: i32 = 0;
    const ERROR_MORE_DATA: i32 = 234;
    const ERROR_NO_MORE_ITEMS: i32 = 259;
    const REG_SZ: u32 = 1;
    const REG_EXPAND_SZ: u32 = 2;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegOpenKeyExW(hkey: Hkey, sub_key: *const u16, options: u32, sam: u32, result: *mut Hkey) -> i32;
        fn RegEnumValueW(
            hkey: Hkey,
            index: u32,
            name: *mut u16,
            name_len: *mut u32,
            reserved: *mut u32,
            vtype: *mut u32,
            data: *mut u8,
            data_len: *mut u32,
        ) -> i32;
        fn RegCloseKey(hkey: Hkey) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Enumerate the string values (`REG_SZ` / `REG_EXPAND_SZ`) of `root\sub_key`.
    /// `None` when the key can't be opened (the caller falls back to the process env).
    pub fn read_env_key(root: Hkey, sub_key: &str) -> Option<Vec<RawVar>> {
        let mut hkey: Hkey = 0;
        let sub = wide(sub_key);
        unsafe {
            if RegOpenKeyExW(root, sub.as_ptr(), 0, KEY_READ, &mut hkey) != ERROR_SUCCESS {
                return None;
            }
        }
        let mut vars = Vec::new();
        let mut data: Vec<u8> = vec![0; 32 * 1024];
        let mut index = 0u32;
        loop {
            // Max value-name length is 16383 chars; re-init per iteration (the API
            // mutates the in/out lengths).
            let mut name = vec![0u16; 16384];
            let mut name_len = name.len() as u32;
            let mut vtype = 0u32;
            let mut data_len = data.len() as u32;
            let rc = unsafe {
                RegEnumValueW(
                    hkey,
                    index,
                    name.as_mut_ptr(),
                    &mut name_len,
                    ptr::null_mut(),
                    &mut vtype,
                    data.as_mut_ptr(),
                    &mut data_len,
                )
            };
            if rc == ERROR_MORE_DATA {
                // Grow to the reported requirement (or double, if it reported small)
                // and retry the SAME index.
                let need = (data_len as usize).max(data.len() * 2);
                data.resize(need, 0);
                continue;
            }
            if rc == ERROR_NO_MORE_ITEMS || rc != ERROR_SUCCESS {
                break;
            }
            index += 1;
            if vtype != REG_SZ && vtype != REG_EXPAND_SZ {
                continue; // PATH-style env keys only hold strings; skip anything else
            }
            let nm = String::from_utf16_lossy(&name[..name_len as usize]);
            if nm.is_empty() {
                continue;
            }
            let units: Vec<u16> = data[..data_len as usize]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let mut val = String::from_utf16_lossy(&units);
            while val.ends_with('\0') {
                val.pop();
            }
            vars.push((nm, val, vtype == REG_EXPAND_SZ));
        }
        unsafe {
            RegCloseKey(hkey);
        }
        Some(vars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(pairs: &[(&str, &str, bool)]) -> Vec<RawVar> {
        pairs.iter().map(|(n, v, e)| (n.to_string(), v.to_string(), *e)).collect()
    }

    fn map(pairs: &[(&str, &str)]) -> EnvMap {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn get<'a>(env: &'a EnvMap, name: &str) -> Option<&'a str> {
        env.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn user_var_wins_over_machine_case_insensitively() {
        let machine = raw(&[("JAVA_HOME", "C:\\jdk8", false)]);
        let user = raw(&[("Java_Home", "C:\\jdk21", false)]);
        let env = merge_fresh_env(&machine, &user, &map(&[]));
        assert_eq!(get(&env, "JAVA_HOME"), Some("C:\\jdk21"));
        // exactly one spelling survives
        assert_eq!(env.keys().filter(|k| k.eq_ignore_ascii_case("JAVA_HOME")).count(), 1);
    }

    #[test]
    fn path_is_machine_then_user_concatenated() {
        let machine = raw(&[("Path", "C:\\Windows;C:\\Windows\\system32", false)]);
        let user = raw(&[("PATH", "C:\\Users\\me\\bin", false)]);
        let env = merge_fresh_env(&machine, &user, &map(&[]));
        assert_eq!(
            get(&env, "PATH"),
            Some("C:\\Windows;C:\\Windows\\system32;C:\\Users\\me\\bin")
        );
    }

    #[test]
    fn user_only_path_passes_through() {
        let user = raw(&[("PATH", "C:\\me\\bin", false)]);
        let env = merge_fresh_env(&[], &user, &map(&[]));
        assert_eq!(get(&env, "PATH"), Some("C:\\me\\bin"));
    }

    #[test]
    fn expand_sz_resolves_against_merged_then_process() {
        let machine = raw(&[("SystemRoot", "C:\\Windows", false)]);
        let user = raw(&[
            ("TEMP", "%USERPROFILE%\\AppData\\Local\\Temp", true),
            ("WINDIR2", "%SystemRoot%\\sub", true),
        ]);
        // USERPROFILE only exists in the process env → the fallback lookup.
        let process = map(&[("USERPROFILE", "C:\\Users\\me")]);
        let env = merge_fresh_env(&machine, &user, &process);
        assert_eq!(get(&env, "TEMP"), Some("C:\\Users\\me\\AppData\\Local\\Temp"));
        assert_eq!(get(&env, "WINDIR2"), Some("C:\\Windows\\sub"));
    }

    #[test]
    fn unknown_expand_token_stays_literal() {
        let user = raw(&[("X", "%NOPE%\\bin", true)]);
        let env = merge_fresh_env(&[], &user, &map(&[]));
        assert_eq!(get(&env, "X"), Some("%NOPE%\\bin"));
    }

    #[test]
    fn unpaired_percent_passes_through() {
        let user = raw(&[("X", "100%", true)]);
        let env = merge_fresh_env(&[], &user, &map(&[]));
        assert_eq!(get(&env, "X"), Some("100%"));
    }

    #[test]
    fn non_expand_value_is_never_expanded() {
        let machine = raw(&[("SystemRoot", "C:\\Windows", false)]);
        let user = raw(&[("X", "%SystemRoot%", false)]);
        let env = merge_fresh_env(&machine, &user, &map(&[]));
        assert_eq!(get(&env, "X"), Some("%SystemRoot%"));
    }

    #[test]
    fn process_only_vars_are_layered_in() {
        let machine = raw(&[("Path", "C:\\Windows", false)]);
        let process = map(&[
            ("HYPERPANES_CONTROL_TOKEN", "tok"),
            ("SESSIONNAME", "Console"),
        ]);
        let env = merge_fresh_env(&machine, &[], &process);
        assert_eq!(get(&env, "HYPERPANES_CONTROL_TOKEN"), Some("tok"));
        assert_eq!(get(&env, "SESSIONNAME"), Some("Console"));
    }

    #[test]
    fn fresh_registry_value_beats_stale_process_copy() {
        // The app launched with PATH=stale; the registry now says otherwise.
        let machine = raw(&[("Path", "C:\\fresh", false)]);
        let process = map(&[("PATH", "C:\\stale"), ("Path", "C:\\stale2")]);
        let env = merge_fresh_env(&machine, &[], &process);
        assert_eq!(get(&env, "PATH"), Some("C:\\fresh"));
        assert_eq!(env.keys().filter(|k| k.eq_ignore_ascii_case("PATH")).count(), 1);
    }

    #[test]
    fn empty_machine_path_does_not_lead_with_semicolon() {
        let machine = raw(&[("Path", "", false)]);
        let user = raw(&[("Path", "C:\\me\\bin", false)]);
        let env = merge_fresh_env(&machine, &user, &map(&[]));
        assert_eq!(get(&env, "PATH"), Some("C:\\me\\bin"));
    }

    // Live smoke for the #28 user intent: a user var set AFTER this process started
    // (so it's absent from the process env) must reach a fresh spawn base. Run manually:
    //   [Environment]::SetEnvironmentVariable('HP_TEST','1','User')
    //   cargo test -p hyperpanes-core fresh_env_sees_post_launch_user_var -- --ignored
    //   [Environment]::SetEnvironmentVariable('HP_TEST',$null,'User')
    #[cfg(windows)]
    #[test]
    #[ignore = "requires the HP_TEST user env var to be set out-of-process first"]
    fn fresh_env_sees_post_launch_user_var() {
        assert!(std::env::var("HP_TEST").is_err(), "HP_TEST leaked into the process env");
        assert_eq!(get(&fresh_env(), "HP_TEST"), Some("1"));
    }

    // Live smoke: the real registry read produces a usable base (Windows only).
    #[cfg(windows)]
    #[test]
    fn fresh_env_live_has_path_and_systemroot() {
        let env = fresh_env();
        assert!(get(&env, "PATH").map(|p| !p.is_empty()).unwrap_or(false));
        let sr = get(&env, "SystemRoot").unwrap_or_default();
        assert!(!sr.contains('%'), "SystemRoot should be expanded: {sr}");
    }
}
