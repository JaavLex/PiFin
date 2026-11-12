//! Saved Jellyfin users ("Who's watching?").
//!
//! auth.json keeps every signed-in user. The top-level server/user_id/token/
//! device_id always mirror the active user, so the phone remote (and an older
//! pifin binary) can keep reading it unchanged. Each user has its own device id:
//! Jellyfin ties a token to a device, so users never sign each other out.

use crate::jellyfin::Auth;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Profile {
    pub user_id: String,
    #[serde(default)]
    pub name: String,
    pub token: String,
    pub device_id: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub server: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub users: Vec<Profile>,
}

pub fn path() -> String {
    std::env::var("PIFIN_AUTH").unwrap_or_else(|_| {
        format!("{}/.config/pifin/auth.json", std::env::var("HOME").unwrap_or_default())
    })
}

impl Store {
    pub fn load() -> Option<Store> {
        let mut s: Store = serde_json::from_str(&std::fs::read_to_string(path()).ok()?).ok()?;
        // single-user file from v0.1: becomes the first profile
        if s.users.is_empty() && !s.token.is_empty() {
            s.users.push(Profile { user_id: s.user_id.clone(), name: String::new(), token: s.token.clone(), device_id: s.device_id.clone() });
        }
        (!s.users.is_empty()).then_some(s)
    }

    pub fn save(&self) -> Result<(), String> {
        use std::os::unix::fs::OpenOptionsExt;
        let p = path();
        if let Some(dir) = std::path::Path::new(&p).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = format!("{p}.tmp");
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)
            .and_then(|mut f| std::io::Write::write_all(&mut f, json.as_bytes()))
            .and_then(|_| std::fs::rename(&tmp, &p))
            .map_err(|e| format!("{p}: {e}"))
    }

    /// Make `user_id` the active user (mirrored at the top level).
    pub fn activate(&mut self, user_id: &str) {
        if let Some(p) = self.users.iter().find(|p| p.user_id == user_id).cloned() {
            self.user_id = p.user_id;
            self.token = p.token;
            self.device_id = p.device_id;
        }
    }

    /// Add or refresh a user and make it active.
    pub fn add(&mut self, auth: &Auth, name: &str) {
        self.server = auth.server.clone();
        let p = Profile { user_id: auth.user_id.clone(), name: name.into(), token: auth.token.clone(), device_id: auth.device_id.clone() };
        match self.users.iter_mut().find(|u| u.user_id == p.user_id) {
            Some(u) => *u = p,
            None => self.users.push(p),
        }
        self.activate(&auth.user_id);
    }

    pub fn auth(&self, p: &Profile) -> Auth {
        Auth { server: self.server.clone(), user_id: p.user_id.clone(), token: p.token.clone(), device_id: p.device_id.clone() }
    }
}

/// Restart the app (a clean process per user: no state to reset).
pub fn restart(args: &[&str]) -> ! {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(std::env::current_exe().unwrap_or_default())
        .args(args)
        .env("PIFIN_RESTARTED", "1") // no phone-remote hint again
        .exec();
    eprintln!("restart failed: {err}");
    std::process::exit(1);
}
