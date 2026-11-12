//! Unix-socket IPC server and persisted user state.

use crate::config::{Config, Res};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tpfand_proto::{FanMode, Limits, Request, Response, SOCKET_PATH, Status};

/// Choices made from the panel; persisted across restarts.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct UserState {
    /// None = auto (follow system power mode).
    pub mode: Option<String>,
    pub fan: FanMode,
    pub limits: BTreeMap<String, Limits>,
}

impl UserState {
    pub fn load(path: &str) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &str) {
        let p = Path::new(path);
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let tmp = p.with_extension("tmp");
        let res = serde_json::to_vec_pretty(self)
            .map_err(std::io::Error::other)
            .and_then(|b| fs::write(&tmp, b))
            .and_then(|_| fs::rename(&tmp, p));
        if let Err(e) = res {
            eprintln!("warn: saving state to {path}: {e}");
        }
    }
}

pub struct Shared {
    pub user: UserState,
    pub status: Status,
    /// Set when `user` changes so the control loop reacts immediately.
    pub dirty: bool,
}

pub type SharedRef = Arc<(Mutex<Shared>, Condvar)>;

pub fn serve(cfg: Arc<Config>, shared: SharedRef) -> Res<()> {
    let _ = fs::remove_file(SOCKET_PATH);
    let listener = UnixListener::bind(SOCKET_PATH)?;
    // Everyone may connect (status is read-only); writes are checked per peer.
    fs::set_permissions(SOCKET_PATH, fs::Permissions::from_mode(0o666))?;
    std::thread::Builder::new().name("ipc".into()).spawn(move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let cfg = Arc::clone(&cfg);
            let shared = Arc::clone(&shared);
            let _ = std::thread::Builder::new().name("client".into()).spawn(move || {
                if let Err(e) = handle(stream, &cfg, &shared) {
                    eprintln!("ipc: client error: {e}");
                }
            });
        }
    })?;
    Ok(())
}

fn handle(stream: UnixStream, cfg: &Config, shared: &SharedRef) -> Res<()> {
    let can_write = peer_uid(&stream).is_some_and(|uid| uid_allowed(uid, &cfg.allow_group));
    stream.set_read_timeout(Some(Duration::from_secs(600)))?;
    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Err(e) => Response::Error { message: format!("bad request: {e}") },
            Ok(req) => dispatch(req, can_write, cfg, shared),
        };
        let mut out = serde_json::to_string(&resp)?;
        out.push('\n');
        writer.write_all(out.as_bytes())?;
    }
    Ok(())
}

fn dispatch(req: Request, can_write: bool, cfg: &Config, shared: &SharedRef) -> Response {
    let (lock, cvar) = &**shared;
    let mut sh = lock.lock().unwrap();
    if let Request::Status = req {
        let mut s = sh.status.clone();
        s.can_write = can_write;
        return Response::Status(Box::new(s));
    }
    if !can_write {
        return Response::Error {
            message: format!("permission denied (must be root or in group '{}')", cfg.allow_group),
        };
    }
    match req {
        Request::Status => unreachable!(),
        Request::SetMode { profile } => {
            if let Some(p) = &profile {
                if !cfg.profiles.contains_key(p) {
                    return Response::Error { message: format!("unknown profile '{p}'") };
                }
            }
            sh.user.mode = profile;
        }
        Request::SetFan { fan } => sh.user.fan = fan,
        Request::SetLimits { profile, limits } => {
            if !cfg.profiles.contains_key(&profile) {
                return Response::Error { message: format!("unknown profile '{profile}'") };
            }
            match limits {
                Some(l) => {
                    sh.user.limits.insert(profile, cfg.clamp(l));
                }
                None => {
                    sh.user.limits.remove(&profile);
                }
            }
        }
    }
    sh.user.save(&cfg.state_file);
    sh.dirty = true;
    cvar.notify_all();
    Response::Ok
}

fn peer_uid(s: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: valid fd, correctly sized out-buffer for SO_PEERCRED.
    let rc = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

/// root, or a member (primary or supplementary) of `group`.
fn uid_allowed(uid: u32, group: &str) -> bool {
    if uid == 0 {
        return true;
    }
    let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
    let Some((user, primary_gid)) = passwd.lines().find_map(|l| {
        let f: Vec<_> = l.split(':').collect();
        (f.len() > 3 && f[2].parse() == Ok(uid)).then(|| (f[0].to_string(), f[3].to_string()))
    }) else {
        return false;
    };
    let groups = fs::read_to_string("/etc/group").unwrap_or_default();
    groups.lines().any(|l| {
        let f: Vec<_> = l.split(':').collect();
        f.len() > 3
            && f[0] == group
            && (f[2] == primary_gid || f[3].split(',').any(|m| m == user))
    })
}
