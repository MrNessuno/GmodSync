// Avviatore a file singolo per Garry's Mod Workshop Sync.
//
// Il programma vero ha bisogno della libreria di Steam (steam_api64.dll su
// Windows, libsteam_api.so su Linux) accanto a sé. Per non dover distribuire
// tre file, questo eseguibile li contiene entrambi: al primo avvio li estrae in
// una cartella dell'utente e poi avvia il programma da lì.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

static APP: &[u8] = include_bytes!(env!("GMOD_EXE"));
static LIB: &[u8] = include_bytes!(env!("GMOD_LIB"));

#[cfg(windows)]
const APP_NAME: &str = "gmod-ws-sync-app.exe";
#[cfg(windows)]
const LIB_NAME: &str = "steam_api64.dll";
#[cfg(not(windows))]
const APP_NAME: &str = "gmod-ws-sync-app";
#[cfg(not(windows))]
const LIB_NAME: &str = "libsteam_api.so";

/// Impronta dei contenuti: una versione nuova finisce in una cartella nuova.
fn fnv(data: &[u8], mut h: u64) -> u64 {
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn base_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(d) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(d);
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(d) = std::env::var_os("XDG_CACHE_HOME") {
            return PathBuf::from(d);
        }
        if let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h).join(".cache");
        }
    }
    std::env::temp_dir()
}

/// Scrive il file solo se manca o è diverso, passando da un file temporaneo.
fn put(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Ok(m) = fs::metadata(path) {
        if m.len() == data.len() as u64 {
            return Ok(());
        }
    }
    let tmp = path.with_extension("part");
    fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&tmp, path)
}

fn prepare() -> std::io::Result<PathBuf> {
    let h = fnv(LIB, fnv(APP, 0xcbf29ce484222325));
    let dir = base_dir()
        .join("gmod-ws-sync")
        .join(format!("programma-{h:016x}"));
    fs::create_dir_all(&dir)?;
    put(&dir.join(LIB_NAME), LIB)?;
    put(&dir.join(APP_NAME), APP)?;
    Ok(dir)
}

#[cfg(windows)]
fn show_error(msg: &str) {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }
    let text: Vec<u16> = msg.encode_utf16().chain(Some(0)).collect();
    let cap: Vec<u16> = "Garry's Mod Workshop Sync"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe {
        MessageBoxW(0, text.as_ptr(), cap.as_ptr(), 0x10);
    }
}

#[cfg(not(windows))]
fn show_error(msg: &str) {
    eprintln!("Garry's Mod Workshop Sync: {msg}");
}

fn main() {
    let dir = match prepare() {
        Ok(d) => d,
        Err(e) => {
            show_error(&format!("Non riesco a preparare il programma: {e}"));
            std::process::exit(1);
        }
    };
    let mut cmd = Command::new(dir.join(APP_NAME));
    cmd.args(std::env::args_os().skip(1));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut paths = dir.as_os_str().to_owned();
        if let Some(old) = std::env::var_os("LD_LIBRARY_PATH") {
            paths.push(":");
            paths.push(old);
        }
        cmd.env("LD_LIBRARY_PATH", paths);
        // Sostituisce questo processo con il programma vero.
        let e = cmd.exec();
        show_error(&format!("Non riesco ad avviare il programma: {e}"));
        std::process::exit(1);
    }

    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(s) => std::process::exit(s.code().unwrap_or(0)),
            Err(e) => {
                show_error(&format!("Non riesco ad avviare il programma: {e}"));
                std::process::exit(1);
            }
        }
    }
}
