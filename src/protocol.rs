//! Per-user URL protocol registration in `HKCU\Software\Classes\<scheme>`.
//!
//! Default: register on every startup pointing at the current exe (like the
//! original). `--no-register-protocol` skips it. `--unregister-protocol`
//! removes the key ONLY if its open command points at this exe.

use std::path::Path;

/// Result of [`unregister_if_ours`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnregisterOutcome {
    Removed,
    NotRegistered,
    /// Key exists but points elsewhere (e.g. the original Antigravity); left alone.
    OwnedByOther(String),
}

/// `"<exe>" "%1"`.
pub fn open_command_for(exe: &Path) -> String {
    format!("\"{}\" \"%1\"", exe.display())
}

/// Extract the program path from an open command (quoted or bare first token).
pub fn command_program(cmd: &str) -> &str {
    let c = cmd.trim();
    if let Some(rest) = c.strip_prefix('"') {
        rest.split('"').next().unwrap_or("")
    } else {
        c.split_whitespace().next().unwrap_or("")
    }
}

/// Whether `cmd` launches `exe` (case-insensitive, normalised separators).
pub fn command_points_to(cmd: &str, exe: &Path) -> bool {
    let norm = |s: &str| s.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let prog = command_program(cmd);
    !prog.is_empty() && norm(prog) == norm(&exe.to_string_lossy())
}

#[cfg(windows)]
mod imp {
    use std::io;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn check(rc: u32) -> io::Result<()> {
        if rc == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(rc as i32))
        }
    }

    /// Create `HKCU\<subkey>` and set a REG_SZ value (`None` = default value).
    pub fn set_string(subkey: &str, name: Option<&str>, value: &str) -> io::Result<()> {
        unsafe {
            let mut key: HKEY = std::ptr::null_mut();
            let sk = wide(subkey);
            check(RegCreateKeyExW(
                HKEY_CURRENT_USER,
                sk.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            ))?;
            let data = wide(value);
            let n = name.map(wide);
            let rc = RegSetValueExW(
                key,
                n.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as u32,
            );
            RegCloseKey(key);
            check(rc)
        }
    }

    /// Read the default REG_SZ value of `HKCU\<subkey>`; `Ok(None)` if absent.
    pub fn get_default(subkey: &str) -> io::Result<Option<String>> {
        unsafe {
            let sk = wide(subkey);
            let mut size: u32 = 0;
            let rc = RegGetValueW(
                HKEY_CURRENT_USER,
                sk.as_ptr(),
                std::ptr::null(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            );
            if rc == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            check(rc)?;
            let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
            let mut size2 = (buf.len() * 2) as u32;
            check(RegGetValueW(
                HKEY_CURRENT_USER,
                sk.as_ptr(),
                std::ptr::null(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr() as *mut _,
                &mut size2,
            ))?;
            let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
            Ok(Some(String::from_utf16_lossy(&buf[..len])))
        }
    }

    pub fn delete_tree(subkey: &str) -> io::Result<()> {
        let sk = wide(subkey);
        let rc = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, sk.as_ptr()) };
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        check(rc)?;
        let rc = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, sk.as_ptr()) };
        if rc == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            check(rc)
        }
    }
}

fn class_key(scheme: &str) -> String {
    format!("Software\\Classes\\{scheme}")
}

fn command_key(scheme: &str) -> String {
    format!("Software\\Classes\\{scheme}\\shell\\open\\command")
}

/// Register `<scheme>://` for the current user, pointing at `exe`.
#[cfg(windows)]
pub fn register(scheme: &str, exe: &Path) -> std::io::Result<()> {
    imp::set_string(&class_key(scheme), None, &format!("URL:{scheme}"))?;
    imp::set_string(&class_key(scheme), Some("URL Protocol"), "")?;
    imp::set_string(&command_key(scheme), None, &open_command_for(exe))
}

/// Current open command for `<scheme>`, if registered.
#[cfg(windows)]
pub fn registered_command(scheme: &str) -> std::io::Result<Option<String>> {
    imp::get_default(&command_key(scheme))
}

/// Remove the registration only when it points at `exe`.
#[cfg(windows)]
pub fn unregister_if_ours(scheme: &str, exe: &Path) -> std::io::Result<UnregisterOutcome> {
    match registered_command(scheme)? {
        None => Ok(UnregisterOutcome::NotRegistered),
        Some(cmd) if command_points_to(&cmd, exe) => {
            imp::delete_tree(&class_key(scheme))?;
            Ok(UnregisterOutcome::Removed)
        }
        Some(cmd) => Ok(UnregisterOutcome::OwnedByOther(cmd)),
    }
}

#[cfg(not(windows))]
pub fn register(_scheme: &str, _exe: &Path) -> std::io::Result<()> {
    Ok(())
}
#[cfg(not(windows))]
pub fn registered_command(_scheme: &str) -> std::io::Result<Option<String>> {
    Ok(None)
}
#[cfg(not(windows))]
pub fn unregister_if_ours(_scheme: &str, _exe: &Path) -> std::io::Result<UnregisterOutcome> {
    Ok(UnregisterOutcome::NotRegistered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn command_parsing() {
        let exe = PathBuf::from(r"C:\A B\anticraft.exe");
        let cmd = open_command_for(&exe);
        assert_eq!(cmd, r#""C:\A B\anticraft.exe" "%1""#);
        assert!(command_points_to(&cmd, &exe));
        assert!(command_points_to(r#""c:/a b/ANTICRAFT.exe" "%1""#, &exe));
        assert!(!command_points_to(r#""C:\X\Antigravity.exe" "%1""#, &exe));
        assert_eq!(command_program(r"C:\x.exe %1"), r"C:\x.exe");
    }

    #[cfg(windows)]
    #[test]
    fn register_roundtrip_with_temp_scheme() {
        let scheme = format!("anticraft-test-{:08x}", rand::random::<u32>());
        let ours = PathBuf::from(r"C:\Temp\anticraft-test.exe");
        let other = PathBuf::from(r"C:\Temp\other.exe");

        // Remove the temporary HKCU key even if an assertion below panics.
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = imp::delete_tree(&class_key(&self.0));
            }
        }
        let _cleanup = Cleanup(scheme.clone());

        assert_eq!(unregister_if_ours(&scheme, &ours).unwrap(), UnregisterOutcome::NotRegistered);
        register(&scheme, &other).unwrap();
        assert!(matches!(unregister_if_ours(&scheme, &ours).unwrap(), UnregisterOutcome::OwnedByOther(_)));
        assert!(registered_command(&scheme).unwrap().is_some());

        register(&scheme, &ours).unwrap();
        assert_eq!(registered_command(&scheme).unwrap(), Some(open_command_for(&ours)));
        assert_eq!(unregister_if_ours(&scheme, &ours).unwrap(), UnregisterOutcome::Removed);
        assert_eq!(registered_command(&scheme).unwrap(), None);
    }
}
