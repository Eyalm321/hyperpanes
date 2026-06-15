//! Windows [`FreshEnvProvider`](super::FreshEnvProvider): rebuild the spawn base from the
//! durable registry environment (machine + user, merged by the shared pure core), so a
//! PATH entry or user var set after the app launched still reaches a NEW pane. Moved
//! verbatim from the old single-file `env.rs`.

use super::*;

impl FreshEnvProvider for PlatformEnv {
    fn fresh_env_with_process(&self, process: EnvMap) -> EnvMap {
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
        fn RegOpenKeyExW(
            hkey: Hkey,
            sub_key: *const u16,
            options: u32,
            sam: u32,
            result: *mut Hkey,
        ) -> i32;
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
