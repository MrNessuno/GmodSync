// Garry's Mod Workshop Sync
// Tiene allineate le iscrizioni al Workshop di un gruppo di amici.
// Richiede Steam aperto, con l'account che possiede Garry's Mod.
//
// La lista del gruppo può stare in tre posti (si sceglie in Impostazioni):
//  - una collezione del Workshop di Steam (gli amici sono collaboratori)
//  - un file JSON in una cartella condivisa (Dropbox, Drive, Syncthing...)
//  - un file JSON in un repository GitHub
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use steamworks::{AppId, Client, FileType, PublishedFileId};

const GMOD_APP_ID: u32 = 4000;
const GREEN: Color32 = Color32::from_rgb(60, 160, 80);
const ORANGE: Color32 = Color32::from_rgb(230, 150, 40);
const RED: Color32 = Color32::from_rgb(210, 70, 70);
const BLUE: Color32 = Color32::from_rgb(70, 140, 220);
const HISTORY_MAX: usize = 1000;
const STEAM_API: &str = "https://api.steampowered.com/ISteamRemoteStorage";
const CONFLICT: &str = "conflitto";

// ---------------------------------------------------------------------
// Stato del gruppo
// ---------------------------------------------------------------------

/// present = true: tutti devono averlo. present = false: tutti devono toglierlo.
#[derive(Serialize, Deserialize, Clone, Default)]
struct Entry {
    present: bool,
    title: String,
    by: String,
    ts: u64,
}

/// Una voce dello storico: chi ha aggiunto o tolto cosa, e quando.
#[derive(Serialize, Deserialize, Clone, Default)]
struct HistoryEntry {
    ts: u64,
    by: String,
    added: bool,
    id: String,
    title: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Shared {
    #[serde(default)]
    items: BTreeMap<String, Entry>,
    #[serde(default)]
    history: Vec<HistoryEntry>,
}

fn parse_shared(text: &str) -> Result<Shared, String> {
    if text.trim().is_empty() {
        return Ok(Shared::default());
    }
    serde_json::from_str(text).map_err(|e| format!("lista del gruppo non valida: {e}"))
}

/// Un posto dove salvare la lista come documento JSON (file o GitHub).
trait Backend {
    fn load(&self) -> Result<Shared, String>;
    fn save(&self, shared: &Shared) -> Result<(), String>;
}

struct FileBackend {
    path: PathBuf,
}

impl Backend for FileBackend {
    fn load(&self) -> Result<Shared, String> {
        if !self.path.exists() {
            return Ok(Shared::default());
        }
        let text = std::fs::read_to_string(&self.path).map_err(|e| format!("lettura: {e}"))?;
        let mut shared = parse_shared(&text)?;
        // Se due persone salvano insieme, Dropbox/Drive/Syncthing creano una
        // "copia in conflitto": la uniamo, così nessuna aggiunta va persa.
        for other in self.conflict_copies() {
            merge_shared(&mut shared, other);
        }
        Ok(shared)
    }

    fn save(&self, shared: &Shared) -> Result<(), String> {
        let text = serde_json::to_string_pretty(shared).map_err(|e| e.to_string())?;
        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("scrittura: {e}"))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("rinomina: {e}"))
    }
}

impl FileBackend {
    fn conflict_copies(&self) -> Vec<Shared> {
        let mut out = Vec::new();
        let Some(stem) = self.path.file_stem() else {
            return out;
        };
        let stem = stem.to_string_lossy().to_lowercase();
        let dir = match self.path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return out;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p == self.path {
                continue;
            }
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if !name.ends_with(".json") || !name.starts_with(&stem) {
                continue;
            }
            let rest = &name[stem.len()..];
            if !(rest.contains("conflict") || rest.contains("conflitto") || rest.contains('(')) {
                continue;
            }
            if let Ok(t) = std::fs::read_to_string(&p) {
                if let Ok(s) = parse_shared(&t) {
                    out.push(s);
                }
            }
        }
        out
    }
}

/// Unisce due versioni della lista: per ogni addon vince la modifica più recente.
fn merge_shared(base: &mut Shared, other: Shared) {
    for (id, e) in other.items {
        let newer = match base.items.get(&id) {
            Some(b) => e.ts > b.ts,
            None => true,
        };
        if newer {
            base.items.insert(id, e);
        }
    }
    base.history.extend(other.history);
    base.history.sort_by(|a, b| a.ts.cmp(&b.ts));
    base.history
        .dedup_by(|a, b| a.ts == b.ts && a.id == b.id && a.by == b.by && a.added == b.added);
}

struct GitHubBackend {
    repo: String,
    path: String,
    token: String,
    sha: RefCell<Option<String>>,
}

impl GitHubBackend {
    fn url(&self) -> String {
        format!(
            "https://api.github.com/repos/{}/contents/{}",
            self.repo.trim().trim_matches('/'),
            self.path.trim().trim_start_matches('/')
        )
    }

    fn req(&self, method: &str) -> ureq::Request {
        agent()
            .request(method, &self.url())
            .set("Authorization", &format!("Bearer {}", self.token.trim()))
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28")
            .set("User-Agent", "gmod-ws-sync")
    }
}

fn github_error(code: u16) -> String {
    match code {
        401 => "GitHub: token non valido o scaduto".to_string(),
        403 => "GitHub: il token non ha il permesso su questo repository".to_string(),
        404 => "GitHub: repository non trovato, o il token non ci ha accesso".to_string(),
        409 | 422 => CONFLICT.to_string(),
        c => format!("GitHub: errore {c}"),
    }
}

impl Backend for GitHubBackend {
    fn load(&self) -> Result<Shared, String> {
        let r = self.req("GET").query("t", &now().to_string()).call();
        match r {
            Ok(resp) => {
                let v: serde_json::Value = resp
                    .into_json()
                    .map_err(|e| format!("GitHub: risposta non valida: {e}"))?;
                *self.sha.borrow_mut() = v["sha"].as_str().map(|s| s.to_string());
                let content: String = v["content"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                let bytes = B64
                    .decode(content)
                    .map_err(|e| format!("GitHub: contenuto non leggibile: {e}"))?;
                parse_shared(&String::from_utf8_lossy(&bytes))
            }
            Err(ureq::Error::Status(404, _)) => {
                // Il file non c'è ancora: verrà creato al primo salvataggio.
                *self.sha.borrow_mut() = None;
                Ok(Shared::default())
            }
            Err(ureq::Error::Status(code, _)) => Err(github_error(code)),
            Err(e) => Err(format!("GitHub non raggiungibile: {e}")),
        }
    }

    fn save(&self, shared: &Shared) -> Result<(), String> {
        let text = serde_json::to_string_pretty(shared).map_err(|e| e.to_string())?;
        let mut body = serde_json::json!({
            "message": "Aggiornamento lista Garry's Mod Workshop Sync",
            "content": B64.encode(text.as_bytes()),
        });
        if let Some(sha) = self.sha.borrow().clone() {
            body["sha"] = serde_json::Value::String(sha);
        }
        match self.req("PUT").send_json(body) {
            Ok(resp) => {
                if let Ok(v) = resp.into_json::<serde_json::Value>() {
                    *self.sha.borrow_mut() = v["content"]["sha"].as_str().map(|s| s.to_string());
                }
                Ok(())
            }
            Err(ureq::Error::Status(code, _)) => Err(github_error(code)),
            Err(e) => Err(format!("GitHub non raggiungibile: {e}")),
        }
    }
}

/// Rilegge la lista, applica la modifica e la riscrive. Se nel frattempo
/// qualcun altro l'ha cambiata (conflitto su GitHub) riprova.
fn mutate(backend: &dyn Backend, f: impl Fn(&mut Shared)) -> Result<Shared, String> {
    let mut last = String::new();
    for _ in 0..3 {
        let mut shared = backend.load()?;
        f(&mut shared);
        match backend.save(&shared) {
            Ok(()) => return Ok(shared),
            Err(e) if e == CONFLICT => {
                last = e;
                std::thread::sleep(Duration::from_millis(700));
            }
            Err(e) => return Err(e),
        }
    }
    Err(format!(
        "{last}: la lista è stata cambiata da un altro nello stesso momento, riprova"
    ))
}

// ---------------------------------------------------------------------
// Impostazioni locali
// ---------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    File,
    Steam,
    GitHub,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Steam => "Collezione Steam",
            Mode::File => "Cartella condivisa",
            Mode::GitHub => "Repository GitHub",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Config {
    #[serde(default)]
    mode: Mode,
    #[serde(default)]
    shared_path: String,
    #[serde(default)]
    collection: String,
    #[serde(default)]
    gh_repo: String,
    #[serde(default)]
    gh_path: String,
    #[serde(default)]
    gh_token: String,
    #[serde(default)]
    nickname: String,
    #[serde(default)]
    ignored: BTreeSet<String>,
    // Memoria della collezione Steam, per capire cosa è stato tolto.
    #[serde(default)]
    col_state_for: String,
    #[serde(default)]
    col_seen: BTreeSet<String>,
    #[serde(default)]
    col_present: BTreeSet<String>,
}

fn data_dir() -> PathBuf {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gmod-ws-sync");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

impl Config {
    fn load() -> Config {
        std::fs::read_to_string(data_dir().join("config.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        if let Ok(t) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(data_dir().join("config.json"), t);
        }
    }

    fn collection_id(&self) -> String {
        parse_ids(&self.collection)
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    fn gh_file(&self) -> String {
        if self.gh_path.trim().is_empty() {
            "gmod-workshop-sync.json".to_string()
        } else {
            self.gh_path.trim().to_string()
        }
    }

    fn configured(&self) -> bool {
        match self.mode {
            Mode::File => !self.shared_path.trim().is_empty(),
            Mode::Steam => !self.collection_id().is_empty(),
            Mode::GitHub => !self.gh_repo.trim().is_empty() && !self.gh_token.trim().is_empty(),
        }
    }

    /// Il posto dove sta la lista, se è un documento JSON (file o GitHub).
    fn doc_backend(&self) -> Option<Box<dyn Backend + Send>> {
        match self.mode {
            Mode::File if !self.shared_path.trim().is_empty() => Some(Box::new(FileBackend {
                path: PathBuf::from(self.shared_path.trim()),
            })),
            Mode::GitHub if self.configured() => Some(Box::new(GitHubBackend {
                repo: self.gh_repo.clone(),
                path: self.gh_file(),
                token: self.gh_token.clone(),
                sha: RefCell::new(None),
            })),
            _ => None,
        }
    }
}

fn history_path(col: &str) -> PathBuf {
    data_dir().join(format!("storico-collezione-{col}.json"))
}

fn load_local_history(col: &str) -> Vec<HistoryEntry> {
    std::fs::read_to_string(history_path(col))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_local_history(col: &str, h: &[HistoryEntry]) {
    if let Ok(t) = serde_json::to_string_pretty(h) {
        let _ = std::fs::write(history_path(col), t);
    }
}

// ---------------------------------------------------------------------
// Piccole utilità
// ---------------------------------------------------------------------

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn default_nick() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "anonimo".to_string())
}

/// "adesso", "5 min fa", "3 ore fa", "ieri", "12 giorni fa"
fn ago(ts: u64) -> String {
    if ts == 0 {
        return String::new();
    }
    let d = now().saturating_sub(ts);
    if d < 60 {
        "adesso".to_string()
    } else if d < 3600 {
        format!("{} min fa", d / 60)
    } else if d < 86400 {
        let h = d / 3600;
        if h == 1 {
            "1 ora fa".to_string()
        } else {
            format!("{h} ore fa")
        }
    } else if d < 2 * 86400 {
        "ieri".to_string()
    } else {
        format!("{} giorni fa", d / 86400)
    }
}

fn human_size(b: u64) -> String {
    if b == 0 {
        return String::new();
    }
    let f = b as f64;
    if f >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GB", f / (1024.0 * 1024.0 * 1024.0))
    } else if f >= 1024.0 * 1024.0 {
        format!("{:.0} MB", f / (1024.0 * 1024.0))
    } else {
        format!("{:.0} KB", (f / 1024.0).max(1.0))
    }
}

/// Estrae gli ID del Workshop da un testo (ID nudi o link).
fn parse_ids(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for c in text.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_digit() {
            cur.push(c);
        } else {
            if cur.len() >= 5 && cur.len() <= 20 && !out.contains(&cur) {
                out.push(cur.clone());
            }
            cur.clear();
        }
    }
    out
}

/// Vero se il percorso sembra dentro una cartella che si sincronizza da sola.
fn looks_synced(path: &str) -> bool {
    let p = path.to_lowercase();
    [
        "dropbox",
        "google drive",
        "googledrive",
        "my drive",
        "il mio drive",
        "onedrive",
        "sync",
        "nextcloud",
        "owncloud",
        "mega",
        "icloud",
        "pcloud",
        "insync",
        "seafile",
    ]
    .iter()
    .any(|k| p.contains(k))
}

/// Toglie i tag [b], [url=...], [h1]... dalle descrizioni del Workshop e accorcia il testo.
fn clean_bbcode(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '[' => in_tag = true,
            ']' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let mut res = String::new();
    let mut blank = 0;
    for line in out.lines() {
        let l = line.trim();
        if l.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        res.push_str(l);
        res.push('\n');
    }
    let res = res.trim().to_string();
    if res.chars().count() > 800 {
        let short: String = res.chars().take(800).collect();
        format!("{short}…")
    } else {
        res
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build()
}

fn num(v: &serde_json::Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// Steam: richieste al client (dipendenze, collezioni)
// ---------------------------------------------------------------------

/// Quello che il client Steam ci dice di un oggetto del Workshop.
#[derive(Clone, Default)]
struct UgcInfo {
    id: String,
    title: String,
    is_collection: bool,
    /// Per una collezione: i suoi oggetti. Per un addon: gli "Oggetti richiesti".
    children: Vec<String>,
}

type UgcReply = Sender<Result<Vec<UgcInfo>, String>>;

struct SteamReq {
    ids: Vec<u64>,
    reply: UgcReply,
}

/// Permette ai thread di lavoro di chiedere cose al client Steam, che vive
/// nel thread dell'interfaccia (lì vengono eseguite le callback).
#[derive(Clone)]
struct SteamLink {
    tx: Sender<SteamReq>,
    ctx: egui::Context,
}

impl SteamLink {
    fn query(&self, ids: &[String]) -> Result<BTreeMap<String, UgcInfo>, String> {
        let nums: Vec<u64> = ids.iter().filter_map(|s| s.parse().ok()).collect();
        let mut out = BTreeMap::new();
        for chunk in nums.chunks(50) {
            let (rtx, rrx) = channel();
            self.tx
                .send(SteamReq {
                    ids: chunk.to_vec(),
                    reply: rtx,
                })
                .map_err(|_| "Steam non disponibile".to_string())?;
            self.ctx.request_repaint();
            let r = rrx
                .recv_timeout(Duration::from_secs(30))
                .map_err(|_| "Steam non ha risposto in tempo".to_string())??;
            for i in r {
                out.insert(i.id.clone(), i);
            }
        }
        Ok(out)
    }

    /// Gli oggetti dentro una collezione (e il suo titolo).
    fn collection(&self, col: &str) -> Result<(String, BTreeSet<String>), String> {
        let info = self.query(&[col.to_string()])?;
        match info.get(col) {
            None => Err(
                "collezione non trovata: controlla il link, che sia pubblicata e che tu possa vederla"
                    .to_string(),
            ),
            Some(i) if !i.is_collection => {
                Err("il link indicato è un addon, non una collezione".to_string())
            }
            Some(i) => Ok((i.title.clone(), i.children.iter().cloned().collect())),
        }
    }
}

// ---------------------------------------------------------------------
// Informazioni sugli addon (API web pubblica di Steam + client Steam)
// ---------------------------------------------------------------------

#[derive(Clone, Default)]
struct Meta {
    title: String,
    description: String,
    preview: String,
    ok: bool,
    app: u64,
    vis: u64,
    banned: bool,
    size: u64,
    updated: u64,
    /// Oggetti richiesti da questo addon
    deps: Vec<String>,
}

impl Meta {
    fn deleted(&self) -> bool {
        !self.ok || self.banned
    }

    fn wrong_app(&self) -> bool {
        self.app != 0 && self.app != GMOD_APP_ID as u64
    }

    fn warning(&self) -> String {
        if !self.ok {
            return "eliminato o non trovato".to_string();
        }
        if self.banned {
            return "bannato dal Workshop".to_string();
        }
        if self.wrong_app() {
            return "non è di Garry's Mod".to_string();
        }
        if self.vis == 1 || self.vis == 2 {
            return "non pubblico".to_string();
        }
        String::new()
    }
}

fn fetch_meta(ids: &[String]) -> Result<BTreeMap<String, Meta>, String> {
    let agent = agent();
    let mut out = BTreeMap::new();
    for chunk in ids.chunks(100) {
        let mut form: Vec<(String, String)> =
            vec![("itemcount".to_string(), chunk.len().to_string())];
        for (i, id) in chunk.iter().enumerate() {
            form.push((format!("publishedfileids[{i}]"), id.clone()));
        }
        let pairs: Vec<(&str, &str)> = form.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let resp: serde_json::Value = agent
            .post(&format!("{STEAM_API}/GetPublishedFileDetails/v1/"))
            .send_form(&pairs)
            .map_err(|e| format!("rete: {e}"))?
            .into_json()
            .map_err(|e| format!("risposta non valida: {e}"))?;
        if let Some(list) = resp["response"]["publishedfiledetails"].as_array() {
            for d in list {
                let id = d["publishedfileid"].as_str().unwrap_or("").to_string();
                if id.is_empty() {
                    continue;
                }
                out.insert(
                    id,
                    Meta {
                        title: d["title"].as_str().unwrap_or("").to_string(),
                        description: clean_bbcode(d["description"].as_str().unwrap_or("")),
                        preview: d["preview_url"].as_str().unwrap_or("").to_string(),
                        ok: num(&d["result"]) == 1,
                        app: num(&d["consumer_app_id"]),
                        vis: num(&d["visibility"]),
                        banned: num(&d["banned"]) == 1,
                        size: num(&d["file_size"]),
                        updated: num(&d["time_updated"]),
                        deps: Vec::new(),
                    },
                );
            }
        }
    }
    Ok(out)
}

/// Informazioni complete: dati dal web + oggetti richiesti dal client Steam.
fn fetch_meta_full(link: &SteamLink, ids: &[String]) -> Result<BTreeMap<String, Meta>, String> {
    let mut m = fetch_meta(ids)?;
    if let Ok(info) = link.query(ids) {
        for (id, i) in info {
            if let Some(mm) = m.get_mut(&id) {
                if !i.is_collection {
                    mm.deps = i.children;
                }
            }
        }
    }
    Ok(m)
}

/// Prepara una condivisione: apre le collezioni, aggiunge gli oggetti richiesti
/// (anche indiretti) e scarta quello che non va. Gira in un thread di lavoro.
fn prepare_share(link: &SteamLink, ids: Vec<String>, local: &BTreeSet<String>) -> Msg {
    let mut logs: Vec<String> = Vec::new();
    let mut items: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = ids.iter().cloned().collect();
    let mut deps_of: Vec<(String, String)> = Vec::new(); // (richiesto, da chi)
    let mut frontier = ids;
    for _ in 0..8 {
        if frontier.is_empty() {
            break;
        }
        let info = match link.query(&frontier) {
            Ok(i) => i,
            Err(e) => {
                logs.push(format!(
                    "Non riesco a leggere da Steam gli oggetti richiesti ({e}): li salto"
                ));
                items.append(&mut frontier);
                break;
            }
        };
        let mut next = Vec::new();
        for id in frontier.drain(..) {
            match info.get(&id) {
                Some(i) if i.is_collection => {
                    logs.push(format!(
                        "\"{}\" è una collezione: prendo i suoi {} oggetti",
                        i.title,
                        i.children.len()
                    ));
                    for c in &i.children {
                        if seen.insert(c.clone()) {
                            next.push(c.clone());
                        }
                    }
                }
                Some(i) => {
                    for c in &i.children {
                        if seen.insert(c.clone()) {
                            next.push(c.clone());
                            deps_of.push((c.clone(), id.clone()));
                        }
                    }
                    items.push(id);
                }
                None => items.push(id),
            }
        }
        frontier = next;
    }
    items.append(&mut frontier);

    let metas = match fetch_meta(&items) {
        Ok(m) => m,
        Err(e) => {
            logs.push(format!("Controllo degli addon non riuscito: {e}"));
            BTreeMap::new()
        }
    };
    let title = |id: &str| -> String {
        metas
            .get(id)
            .map(|m| m.title.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| format!("Workshop {id}"))
    };
    for (d, parent) in &deps_of {
        logs.push(format!(
            "Oggetto richiesto aggiunto: {} (serve a {})",
            title(d),
            title(parent)
        ));
    }
    let mut good = Vec::new();
    for id in items {
        match metas.get(&id) {
            Some(m) if m.deleted() || m.wrong_app() => {
                logs.push(format!("Scartato {}: {}", title(&id), m.warning()));
            }
            None if !local.contains(&id) => {
                logs.push(format!("Scartato {id}: ID sconosciuto"));
            }
            _ => good.push(id),
        }
    }
    Msg::ShareReady { good, metas, logs }
}

// ---------------------------------------------------------------------
// Messaggi dai thread di lavoro
// ---------------------------------------------------------------------

enum Loaded {
    Doc(Shared),
    Collection(String, BTreeSet<String>),
}

enum Msg {
    Loaded {
        gen: u64,
        result: Result<Loaded, String>,
    },
    Meta(Result<BTreeMap<String, Meta>, String>),
    ShareReady {
        good: Vec<String>,
        metas: BTreeMap<String, Meta>,
        logs: Vec<String>,
    },
    Saved {
        gen: u64,
        result: Result<Shared, String>,
        ids: Vec<String>,
        present: bool,
        then: Vec<String>,
    },
    Verify {
        gen: u64,
        result: Result<(String, BTreeSet<String>), String>,
        ids: Vec<String>,
        present: bool,
    },
}

/// Esito di una iscrizione/disiscrizione, che Steam comunica in modo asincrono.
struct Done {
    id: u64,
    what: &'static str,
    result: Result<(), String>,
}

// ---------------------------------------------------------------------
// Righe, filtri, ordinamento, piano di sincronizzazione, azioni
// ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Missing,   // lo ha il gruppo, tu no
    Remove,    // il gruppo l'ha tolto, tu ce l'hai ancora
    LocalOnly, // ce l'hai solo tu
    Ok,
    Ignored,
}

fn status_style(s: Status) -> (&'static str, Color32) {
    match s {
        Status::Missing => ("Manca a te", ORANGE),
        Status::Remove => ("Tolto dal gruppo", RED),
        Status::LocalOnly => ("Solo tuo", BLUE),
        Status::Ok => ("A posto", GREEN),
        Status::Ignored => ("Tenuto per te", Color32::GRAY),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Todo,
    LocalOnly,
    Missing,
    Remove,
    Ok,
    Ignored,
    Problems,
    Deleted,
}

impl Filter {
    const ALL: [Filter; 9] = [
        Filter::All,
        Filter::Todo,
        Filter::LocalOnly,
        Filter::Missing,
        Filter::Remove,
        Filter::Ok,
        Filter::Ignored,
        Filter::Problems,
        Filter::Deleted,
    ];

    fn label(self) -> &'static str {
        match self {
            Filter::All => "Tutti",
            Filter::Todo => "Da sistemare con la sincronizzazione",
            Filter::LocalOnly => "Solo tuoi (non condivisi)",
            Filter::Missing => "Mancano a te",
            Filter::Remove => "Tolti dal gruppo",
            Filter::Ok => "A posto",
            Filter::Ignored => "Tenuti per te",
            Filter::Problems => "Con avvisi",
            Filter::Deleted => "Eliminati o non trovati",
        }
    }

    fn matches(self, r: &Row) -> bool {
        match self {
            Filter::All => true,
            Filter::Todo => matches!(r.status, Status::Missing | Status::Remove),
            Filter::LocalOnly => r.status == Status::LocalOnly,
            Filter::Missing => r.status == Status::Missing,
            Filter::Remove => r.status == Status::Remove,
            Filter::Ok => r.status == Status::Ok,
            Filter::Ignored => r.status == Status::Ignored,
            Filter::Problems => !r.warn.is_empty(),
            Filter::Deleted => r.deleted,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortBy {
    Status,
    Name,
    Size,
    Recent,
}

impl SortBy {
    const ALL: [SortBy; 4] = [SortBy::Status, SortBy::Name, SortBy::Size, SortBy::Recent];

    fn label(self) -> &'static str {
        match self {
            SortBy::Status => "Stato",
            SortBy::Name => "Nome",
            SortBy::Size => "Peso (prima i più grandi)",
            SortBy::Recent => "Ultime modifiche del gruppo",
        }
    }
}

#[derive(Clone)]
struct Row {
    id: String,
    title: String,
    status: Status,
    warn: String,
    deleted: bool,
    size: u64,
    ts: u64,
}

#[derive(Default)]
struct Plan {
    // (id, titolo, peso, spuntato)
    subscribe: Vec<(String, String, u64, bool)>,
    unsubscribe: Vec<(String, String, u64, bool)>,
}

enum Action {
    Refresh,
    Sync,
    ApplyPlan,
    ClosePlan,
    ShareSelected,
    AskRemove,
    ConfirmRemove,
    CancelRemove,
    ToggleIgnore,
    AddIds,
    OpenSettings(Mode),
    SaveSettings,
}

/// Bozza della finestra Impostazioni.
#[derive(Clone)]
struct Draft {
    mode: Mode,
    path: String,
    collection: String,
    gh_repo: String,
    gh_path: String,
    gh_token: String,
    nick: String,
}

// ---------------------------------------------------------------------
// Applicazione
// ---------------------------------------------------------------------

struct App {
    ctx: egui::Context,
    client: Option<Client>,
    steam_error: String,
    cfg: Config,
    gen: u64,
    shared: Shared,
    shared_error: String,
    collection_title: String,
    loaded_once: bool,
    local: Vec<u64>,
    meta: BTreeMap<String, Meta>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    busy: usize,
    link: SteamLink,
    steam_req_rx: Receiver<SteamReq>,
    steam_tx: Sender<Done>,
    steam_rx: Receiver<Done>,
    pending: usize,
    selected: BTreeSet<String>,
    current: Option<String>,
    search: String,
    filter: Filter,
    sort: SortBy,
    add_input: String,
    want_plan: bool,
    plan: Option<Plan>,
    confirm_remove: Option<Vec<String>>,
    show_history: bool,
    settings: Option<Draft>,
    log: Vec<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let cfg = Config::load();
        let (client, steam_error) = match Client::init_app(AppId(GMOD_APP_ID)) {
            Ok(c) => (Some(c), String::new()),
            Err(e) => (
                None,
                format!(
                    "Steam non risponde: apri Steam con l'account che ha Garry's Mod e riavvia il programma ({e:?})"
                ),
            ),
        };
        let (tx, rx) = channel();
        let (req_tx, steam_req_rx) = channel();
        let (steam_tx, steam_rx) = channel();
        let mut app = App {
            ctx: cc.egui_ctx.clone(),
            client,
            steam_error,
            cfg,
            gen: 0,
            shared: Shared::default(),
            shared_error: String::new(),
            collection_title: String::new(),
            loaded_once: false,
            local: Vec::new(),
            meta: BTreeMap::new(),
            tx,
            rx,
            busy: 0,
            link: SteamLink {
                tx: req_tx,
                ctx: cc.egui_ctx.clone(),
            },
            steam_req_rx,
            steam_tx,
            steam_rx,
            pending: 0,
            selected: BTreeSet::new(),
            current: None,
            search: String::new(),
            filter: Filter::All,
            sort: SortBy::Status,
            add_input: String::new(),
            want_plan: false,
            plan: None,
            confirm_remove: None,
            show_history: false,
            settings: None,
            log: Vec::new(),
        };
        app.reload_all(true);
        app
    }

    /// Esegue un lavoro in un thread e ne riceve il risultato come messaggio.
    fn spawn(&mut self, f: impl FnOnce() -> Msg + Send + 'static) {
        self.busy += 1;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let m = f();
            let _ = tx.send(m);
            ctx.request_repaint();
        });
    }

    // ----- dati -----

    fn nick(&self) -> String {
        if self.cfg.nickname.trim().is_empty() {
            default_nick()
        } else {
            self.cfg.nickname.trim().to_string()
        }
    }

    fn local_set(&self) -> BTreeSet<String> {
        self.local.iter().map(|i| i.to_string()).collect()
    }

    fn group_set(&self) -> BTreeSet<String> {
        self.shared
            .items
            .iter()
            .filter(|(_, e)| e.present)
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn reload_all(&mut self, full_meta: bool) {
        self.refresh_local();
        self.reload_shared();
        self.start_meta(full_meta);
    }

    fn refresh_local(&mut self) {
        if let Some(c) = &self.client {
            let mut v: Vec<u64> = c.ugc().subscribed_items(true).iter().map(|i| i.0).collect();
            v.sort_unstable();
            v.dedup();
            self.local = v;
        }
    }

    fn reload_shared(&mut self) {
        if !self.cfg.configured() {
            self.shared = Shared::default();
            self.shared_error.clear();
            return;
        }
        let gen = self.gen;
        match self.cfg.mode {
            Mode::Steam => {
                let link = self.link.clone();
                let col = self.cfg.collection_id();
                self.spawn(move || Msg::Loaded {
                    gen,
                    result: link
                        .collection(&col)
                        .map(|(t, set)| Loaded::Collection(t, set)),
                });
            }
            _ => {
                let cfg = self.cfg.clone();
                self.spawn(move || {
                    let result = match cfg.doc_backend() {
                        Some(b) => b.load().map(Loaded::Doc),
                        None => Err("impostazioni incomplete".to_string()),
                    };
                    Msg::Loaded { gen, result }
                });
            }
        }
    }

    /// Scarica titoli, immagini, pesi e oggetti richiesti. Se full è falso,
    /// solo per gli addon di cui non sappiamo ancora niente.
    fn start_meta(&mut self, full: bool) {
        let mut ids = self.local_set();
        ids.extend(self.group_set());
        let ids: Vec<String> = ids
            .into_iter()
            .filter(|i| full || !self.meta.contains_key(i))
            .collect();
        if ids.is_empty() {
            return;
        }
        let link = self.link.clone();
        self.spawn(move || Msg::Meta(fetch_meta_full(&link, &ids)));
    }

    fn title_of(&self, id: &str) -> String {
        if let Some(m) = self.meta.get(id) {
            if !m.title.is_empty() {
                return m.title.clone();
            }
        }
        if let Some(e) = self.shared.items.get(id) {
            if !e.title.is_empty() {
                return e.title.clone();
            }
        }
        format!("Workshop {id}")
    }

    fn size_of(&self, id: &str) -> u64 {
        self.meta.get(id).map(|m| m.size).unwrap_or(0)
    }

    fn build_rows(&self) -> Vec<Row> {
        let local = self.local_set();
        let group = self.group_set();
        let mut ids = local.clone();
        ids.extend(group.iter().cloned());
        let mut rows = Vec::new();
        for id in ids {
            let entry = self.shared.items.get(&id);
            let have = local.contains(&id);
            let status = if self.cfg.ignored.contains(&id) {
                Status::Ignored
            } else {
                match (have, entry) {
                    (true, Some(e)) if e.present => Status::Ok,
                    (true, Some(_)) => Status::Remove,
                    (true, None) => Status::LocalOnly,
                    (false, _) => Status::Missing,
                }
            };
            let meta = self.meta.get(&id);
            let mut warn = meta.map(|m| m.warning()).unwrap_or_default();
            if warn.is_empty() {
                if let Some(m) = meta {
                    let miss: Vec<&String> = m
                        .deps
                        .iter()
                        .filter(|d| !group.contains(*d) && !local.contains(*d))
                        .collect();
                    if !miss.is_empty() {
                        warn = format!("mancano {} oggetti richiesti", miss.len());
                    }
                }
            }
            rows.push(Row {
                title: self.title_of(&id),
                deleted: meta.map(|m| m.deleted()).unwrap_or(false),
                size: meta.map(|m| m.size).unwrap_or(0),
                ts: entry.map(|e| e.ts).unwrap_or(0),
                warn,
                status,
                id,
            });
        }
        match self.sort {
            SortBy::Status => rows.sort_by(|a, b| {
                a.status
                    .cmp(&b.status)
                    .then(a.title.to_lowercase().cmp(&b.title.to_lowercase()))
            }),
            SortBy::Name => rows.sort_by_key(|r| r.title.to_lowercase()),
            SortBy::Size => rows.sort_by(|a, b| b.size.cmp(&a.size)),
            SortBy::Recent => rows.sort_by(|a, b| b.ts.cmp(&a.ts)),
        }
        rows
    }

    fn make_plan(&self) -> Plan {
        let mut p = Plan::default();
        for r in self.build_rows() {
            match r.status {
                Status::Missing => p.subscribe.push((r.id, r.title, r.size, !r.deleted)),
                Status::Remove => p.unsubscribe.push((r.id, r.title, r.size, true)),
                _ => {}
            }
        }
        p
    }

    // ----- Steam -----

    fn subscribe(&mut self, id: &str) {
        let Ok(n) = id.parse::<u64>() else { return };
        let Some(c) = &self.client else { return };
        let tx = self.steam_tx.clone();
        c.ugc().subscribe_item(PublishedFileId(n), move |r| {
            let _ = tx.send(Done {
                id: n,
                what: "iscrizione",
                result: r.map_err(|e| format!("{e:?}")),
            });
        });
        self.pending += 1;
    }

    fn unsubscribe(&mut self, id: &str) {
        let Ok(n) = id.parse::<u64>() else { return };
        let Some(c) = &self.client else { return };
        let tx = self.steam_tx.clone();
        c.ugc().unsubscribe_item(PublishedFileId(n), move |r| {
            let _ = tx.send(Done {
                id: n,
                what: "disiscrizione",
                result: r.map_err(|e| format!("{e:?}")),
            });
        });
        self.pending += 1;
    }

    /// Risponde alle richieste dei thread di lavoro interrogando il client Steam.
    fn serve_steam_requests(&mut self) {
        while let Ok(req) = self.steam_req_rx.try_recv() {
            let Some(c) = &self.client else {
                let _ = req.reply.send(Err("Steam non collegato".to_string()));
                continue;
            };
            let items: Vec<PublishedFileId> = req.ids.iter().map(|i| PublishedFileId(*i)).collect();
            let q = match c.ugc().query_items(items) {
                Ok(q) => q,
                Err(e) => {
                    let _ = req.reply.send(Err(format!("{e}")));
                    continue;
                }
            };
            let reply = req.reply;
            q.include_children(true)
                .allow_cached_response(0)
                .fetch(move |r| {
                    let res = match r {
                        Ok(qr) => {
                            let mut v = Vec::new();
                            for i in 0..qr.returned_results() {
                                let Some(info) = qr.get(i) else { continue };
                                let kids = qr.get_children(i).unwrap_or_default();
                                v.push(UgcInfo {
                                    id: info.published_file_id.0.to_string(),
                                    title: info.title.clone(),
                                    is_collection: matches!(info.file_type, FileType::Collection),
                                    children: kids.into_iter().map(|k| k.0.to_string()).collect(),
                                });
                            }
                            Ok(v)
                        }
                        Err(e) => Err(format!("{e:?}")),
                    };
                    let _ = reply.send(res);
                });
        }
    }

    fn backup(&mut self) {
        let text: Vec<String> = self.local.iter().map(|i| i.to_string()).collect();
        let path = data_dir().join(format!("backup-{}.txt", now()));
        match std::fs::write(&path, text.join("\n")) {
            Ok(()) => self
                .log
                .push(format!("Backup delle tue iscrizioni: {}", path.display())),
            Err(e) => self.log.push(format!("Backup non riuscito: {e}")),
        }
    }

    // ----- ricezione messaggi -----

    fn pump(&mut self, ctx: &egui::Context) {
        if let Some(c) = &self.client {
            c.run_callbacks();
        }
        self.serve_steam_requests();

        let mut finished = false;
        while let Ok(d) = self.steam_rx.try_recv() {
            let title = self.title_of(&d.id.to_string());
            let line = match d.result {
                Ok(()) => format!("✔ {}: {}", d.what, title),
                Err(e) => format!("✖ {} fallita ({}): {}", d.what, title, e),
            };
            self.log.push(line);
            self.pending = self.pending.saturating_sub(1);
            finished = true;
        }
        if finished && self.pending == 0 {
            self.refresh_local();
        }

        while let Ok(m) = self.rx.try_recv() {
            self.busy = self.busy.saturating_sub(1);
            self.on_msg(m);
        }

        // Le callback di Steam vanno lette spesso anche senza input dell'utente.
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Loaded { gen, result } => {
                if gen != self.gen {
                    return;
                }
                match result {
                    Ok(Loaded::Doc(s)) => {
                        self.shared = s;
                        self.shared_error.clear();
                    }
                    Ok(Loaded::Collection(title, set)) => {
                        self.shared_error.clear();
                        self.collection_title = title;
                        self.on_collection(set);
                    }
                    Err(e) => {
                        self.shared_error = e;
                        self.want_plan = false;
                        return;
                    }
                }
                self.loaded_once = true;
                self.start_meta(false);
                if self.want_plan {
                    self.want_plan = false;
                    self.plan = Some(self.make_plan());
                }
            }
            Msg::Meta(r) => match r {
                Ok(m) => self.meta.extend(m),
                Err(e) => self
                    .log
                    .push(format!("Informazioni sugli addon non scaricate: {e}")),
            },
            Msg::ShareReady { good, metas, logs } => {
                self.meta.extend(metas);
                self.log.extend(logs);
                if good.is_empty() {
                    self.log.push("Niente da condividere.".to_string());
                    return;
                }
                let group = self.group_set();
                let new: Vec<String> = good
                    .iter()
                    .filter(|i| !group.contains(*i))
                    .cloned()
                    .collect();
                if new.is_empty() {
                    self.log
                        .push("Erano già tutti nella lista del gruppo.".to_string());
                    self.after_change(&good, true);
                } else {
                    self.publish(new, true, good);
                }
            }
            Msg::Saved {
                gen,
                result,
                ids,
                present,
                then,
            } => match result {
                Ok(s) => {
                    if gen == self.gen {
                        self.shared = s;
                    }
                    self.log_change(ids.len(), present);
                    self.after_change(&then, present);
                }
                Err(e) => self
                    .log
                    .push(format!("✖ Lista del gruppo non aggiornata: {e}")),
            },
            Msg::Verify {
                gen,
                result,
                ids,
                present,
            } => {
                if gen != self.gen {
                    return;
                }
                match result {
                    Ok((title, set)) => {
                        let wrong: Vec<String> = ids
                            .iter()
                            .filter(|i| set.contains(*i) != present)
                            .map(|i| self.title_of(i))
                            .collect();
                        if wrong.is_empty() {
                            self.log.push(format!(
                                "✔ Steam ha confermato {} modifiche alla collezione",
                                ids.len()
                            ));
                        } else {
                            self.log.push(format!(
                                "✖ Steam non mostra ancora la modifica per: {}. Premi Aggiorna tra poco; se resta così, controlla di essere collaboratore della collezione e che sia pubblicata.",
                                wrong.join(", ")
                            ));
                        }
                        self.collection_title = title;
                        self.on_collection(set);
                    }
                    Err(e) => self
                        .log
                        .push(format!("Non riesco a ricontrollare la collezione: {e}")),
                }
            }
        }
    }

    /// Nuovo contenuto della collezione Steam: lo trasforma nella lista del gruppo,
    /// ricordando cosa c'era prima per capire cosa è stato tolto.
    fn on_collection(&mut self, present: BTreeSet<String>) {
        let col = self.cfg.collection_id();
        let first = self.cfg.col_state_for != col;
        if first {
            self.cfg.col_state_for = col.clone();
            self.cfg.col_seen.clear();
            self.cfg.col_present.clear();
        }
        // Protezione: se Steam risponde con una collezione vuota mentre prima
        // aveva oggetti, è quasi certamente un errore di lettura. Non la usiamo,
        // altrimenti tutto risulterebbe "tolto dal gruppo".
        if !first && present.is_empty() && self.cfg.col_present.len() > 1 {
            self.shared_error = "Steam ha restituito la collezione vuota: la ignoro e tengo l'ultima lista buona. Premi Aggiorna tra poco.".to_string();
            return;
        }
        let mut history = load_local_history(&col);
        let t = now();
        if !first {
            let prev = self.cfg.col_present.clone();
            for id in present.difference(&prev) {
                history.push(HistoryEntry {
                    ts: t,
                    by: "qualcuno".to_string(),
                    added: true,
                    id: id.clone(),
                    title: self.title_of(id),
                });
            }
            for id in prev.difference(&present) {
                history.push(HistoryEntry {
                    ts: t,
                    by: "qualcuno".to_string(),
                    added: false,
                    id: id.clone(),
                    title: self.title_of(id),
                });
            }
            if history.len() > HISTORY_MAX {
                let extra = history.len() - HISTORY_MAX;
                history.drain(0..extra);
            }
            save_local_history(&col, &history);
        }
        self.cfg.col_seen.extend(present.iter().cloned());
        self.cfg.col_present = present.clone();
        self.cfg.save();

        let mut items = BTreeMap::new();
        for id in &self.cfg.col_seen {
            let last = history.iter().rev().find(|h| &h.id == id);
            items.insert(
                id.clone(),
                Entry {
                    present: present.contains(id),
                    title: self.title_of(id),
                    by: last.map(|h| h.by.clone()).unwrap_or_default(),
                    ts: last.map(|h| h.ts).unwrap_or(0),
                },
            );
        }
        self.shared = Shared { items, history };
    }

    fn log_change(&mut self, n: usize, present: bool) {
        if present {
            self.log
                .push(format!("✔ Aggiunti alla lista del gruppo: {n}"));
        } else {
            self.log
                .push(format!("✔ Tolti dalla lista del gruppo: {n}"));
        }
    }

    /// Dopo che la lista del gruppo è cambiata: allinea le tue iscrizioni.
    fn after_change(&mut self, ids: &[String], present: bool) {
        let local = self.local_set();
        for id in ids {
            self.cfg.ignored.remove(id);
            if present && !local.contains(id) {
                self.subscribe(id);
            }
            if !present && local.contains(id) {
                self.unsubscribe(id);
            }
        }
        self.cfg.save();
    }

    // ----- modifica della lista del gruppo -----

    /// Aggiunge (present = true) o toglie gli addon dalla lista del gruppo;
    /// se va a buon fine, poi allinea le tue iscrizioni per gli ID in `then`.
    fn publish(&mut self, ids: Vec<String>, present: bool, then: Vec<String>) {
        if !self.cfg.configured() {
            self.log
                .push("Prima scegli dove tenere la lista del gruppo (Impostazioni).".to_string());
            return;
        }
        let by = self.nick();
        match self.cfg.mode {
            Mode::Steam => self.publish_collection(ids, present, then, by),
            _ => {
                let cfg = self.cfg.clone();
                let gen = self.gen;
                let items: Vec<(String, String)> =
                    ids.iter().map(|i| (i.clone(), self.title_of(i))).collect();
                self.spawn(move || {
                    let result = match cfg.doc_backend() {
                        Some(b) => mutate(b.as_ref(), |s| {
                            let t = now();
                            for (id, title) in &items {
                                s.history.push(HistoryEntry {
                                    ts: t,
                                    by: by.clone(),
                                    added: present,
                                    id: id.clone(),
                                    title: title.clone(),
                                });
                                s.items.insert(
                                    id.clone(),
                                    Entry {
                                        present,
                                        title: title.clone(),
                                        by: by.clone(),
                                        ts: t,
                                    },
                                );
                            }
                            if s.history.len() > HISTORY_MAX {
                                let extra = s.history.len() - HISTORY_MAX;
                                s.history.drain(0..extra);
                            }
                        }),
                        None => Err("impostazioni incomplete".to_string()),
                    };
                    Msg::Saved {
                        gen,
                        result,
                        ids,
                        present,
                        then,
                    }
                });
            }
        }
    }

    /// Collezione Steam: chiede a Steam di aggiungere/togliere gli oggetti,
    /// poi dopo qualche secondo ricontrolla che sia successo davvero.
    fn publish_collection(
        &mut self,
        ids: Vec<String>,
        present: bool,
        then: Vec<String>,
        by: String,
    ) {
        let Ok(col) = self.cfg.collection_id().parse::<u64>() else {
            self.log
                .push("Link della collezione non valido.".to_string());
            return;
        };
        if self.client.is_none() {
            self.log
                .push("Steam non è collegato: non posso modificare la collezione.".to_string());
            return;
        }
        unsafe {
            let ugc = steamworks::sys::SteamAPI_SteamUGC_v021();
            for id in &ids {
                if let Ok(n) = id.parse::<u64>() {
                    if present {
                        steamworks::sys::SteamAPI_ISteamUGC_AddDependency(ugc, col, n);
                    } else {
                        steamworks::sys::SteamAPI_ISteamUGC_RemoveDependency(ugc, col, n);
                    }
                }
            }
        }
        // Lo storico lo scriviamo subito con il tuo nome, così il controllo
        // successivo non lo attribuisce a "qualcuno".
        let col_s = col.to_string();
        let mut history = load_local_history(&col_s);
        let t = now();
        for id in &ids {
            history.push(HistoryEntry {
                ts: t,
                by: by.clone(),
                added: present,
                id: id.clone(),
                title: self.title_of(id),
            });
            if present {
                self.cfg.col_present.insert(id.clone());
                self.cfg.col_seen.insert(id.clone());
            } else {
                self.cfg.col_present.remove(id);
            }
            let title = self.title_of(id);
            if let Some(e) = self.shared.items.get_mut(id) {
                e.present = present;
                e.by = by.clone();
                e.ts = t;
            } else {
                self.shared.items.insert(
                    id.clone(),
                    Entry {
                        present,
                        title,
                        by: by.clone(),
                        ts: t,
                    },
                );
            }
        }
        save_local_history(&col_s, &history);
        self.shared.history = history;
        self.cfg.save();
        self.log.push(format!(
            "Chiesto a Steam di {} {} oggetti nella collezione, ricontrollo tra poco...",
            if present { "aggiungere" } else { "togliere" },
            ids.len()
        ));
        self.after_change(&then, present);

        let link = self.link.clone();
        let gen = self.gen;
        self.spawn(move || {
            std::thread::sleep(Duration::from_secs(5));
            Msg::Verify {
                gen,
                result: link.collection(&col_s),
                ids,
                present,
            }
        });
    }

    fn start_share(&mut self, ids: Vec<String>) {
        if !self.cfg.configured() {
            self.log
                .push("Prima scegli dove tenere la lista del gruppo (Impostazioni).".to_string());
            return;
        }
        self.log
            .push("Controllo gli addon e gli oggetti richiesti...".to_string());
        let link = self.link.clone();
        let local = self.local_set();
        self.spawn(move || prepare_share(&link, ids, &local));
    }

    // ----- azioni -----

    fn run(&mut self, a: Action) {
        match a {
            Action::Refresh => {
                self.reload_all(true);
                self.log.push("Aggiorno la lista...".to_string());
            }
            Action::Sync => {
                self.want_plan = true;
                self.reload_all(false);
            }
            Action::ApplyPlan => {
                if let Some(plan) = self.plan.take() {
                    self.backup();
                    for (id, _, _, on) in plan.subscribe {
                        if on {
                            self.subscribe(&id);
                        }
                    }
                    for (id, _, _, on) in plan.unsubscribe {
                        if on {
                            self.unsubscribe(&id);
                        }
                    }
                }
            }
            Action::ClosePlan => self.plan = None,
            Action::ShareSelected => {
                let ids: Vec<String> = self.selected.iter().cloned().collect();
                if !ids.is_empty() {
                    self.start_share(ids);
                    self.selected.clear();
                }
            }
            Action::AskRemove => {
                let ids: Vec<String> = self.selected.iter().cloned().collect();
                if !ids.is_empty() {
                    self.confirm_remove = Some(ids);
                }
            }
            Action::CancelRemove => self.confirm_remove = None,
            Action::ConfirmRemove => {
                if let Some(ids) = self.confirm_remove.take() {
                    self.backup();
                    self.publish(ids.clone(), false, ids);
                    self.selected.clear();
                }
            }
            Action::ToggleIgnore => {
                let all_ignored = self.selected.iter().all(|id| self.cfg.ignored.contains(id));
                for id in self.selected.clone() {
                    if all_ignored {
                        self.cfg.ignored.remove(&id);
                    } else {
                        self.cfg.ignored.insert(id);
                    }
                }
                self.cfg.save();
                self.selected.clear();
            }
            Action::AddIds => {
                let ids = parse_ids(&self.add_input);
                if ids.is_empty() {
                    self.log
                        .push("Non trovo nessun link o ID valido nel campo.".to_string());
                } else {
                    self.start_share(ids);
                    self.add_input.clear();
                }
            }
            Action::OpenSettings(mode) => {
                self.settings = Some(Draft {
                    mode,
                    path: self.cfg.shared_path.clone(),
                    collection: self.cfg.collection.clone(),
                    gh_repo: self.cfg.gh_repo.clone(),
                    gh_path: self.cfg.gh_file(),
                    gh_token: self.cfg.gh_token.clone(),
                    nick: self.cfg.nickname.clone(),
                });
            }
            Action::SaveSettings => {
                let Some(d) = self.settings.take() else {
                    return;
                };
                self.cfg.mode = d.mode;
                self.cfg.shared_path = d.path.trim().to_string();
                self.cfg.collection = d.collection.trim().to_string();
                self.cfg.gh_repo = d.gh_repo.trim().to_string();
                self.cfg.gh_path = d.gh_path.trim().to_string();
                self.cfg.gh_token = d.gh_token.trim().to_string();
                self.cfg.nickname = d.nick.trim().to_string();
                self.cfg.save();
                if self.cfg.mode == Mode::File && !self.cfg.shared_path.is_empty() {
                    let b = FileBackend {
                        path: PathBuf::from(&self.cfg.shared_path),
                    };
                    if !b.path.exists() {
                        match b.save(&Shared::default()) {
                            Ok(()) => self
                                .log
                                .push(format!("Creato il file del gruppo: {}", b.path.display())),
                            Err(e) => self
                                .log
                                .push(format!("Non riesco a creare il file del gruppo: {e}")),
                        }
                    }
                }
                self.gen += 1;
                self.shared = Shared::default();
                self.collection_title.clear();
                self.loaded_once = false;
                self.reload_all(false);
            }
        }
    }

    // ----- disegno -----

    fn place_text(&self) -> (String, String) {
        match self.cfg.mode {
            Mode::Steam => {
                let t = if self.collection_title.is_empty() {
                    self.cfg.collection_id()
                } else {
                    self.collection_title.clone()
                };
                (
                    format!("collezione Steam «{t}»"),
                    self.cfg.collection.clone(),
                )
            }
            Mode::File => {
                let p = PathBuf::from(&self.cfg.shared_path);
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                (format!("file {name}"), self.cfg.shared_path.clone())
            }
            Mode::GitHub => (
                format!("GitHub {}", self.cfg.gh_repo),
                format!("{} / {}", self.cfg.gh_repo, self.cfg.gh_file()),
            ),
        }
    }

    fn header_panel(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if self.client.is_some() {
                    ui.label(RichText::new("✔ Steam collegato").color(GREEN));
                } else {
                    ui.label(RichText::new("✖ Steam non collegato").color(RED))
                        .on_hover_text(self.steam_error.as_str());
                }
                if self.cfg.configured() {
                    ui.separator();
                    let (short, full) = self.place_text();
                    ui.label("Lista del gruppo:");
                    ui.label(RichText::new(short).strong()).on_hover_text(full);
                    if self.cfg.mode == Mode::Steam {
                        ui.hyperlink_to(
                            "apri",
                            format!(
                                "https://steamcommunity.com/sharedfiles/filedetails/?id={}",
                                self.cfg.collection_id()
                            ),
                        );
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("Impostazioni")
                        .on_hover_text("Dove sta la lista del gruppo e il tuo nome")
                        .clicked()
                    {
                        actions.push(Action::OpenSettings(self.cfg.mode));
                    }
                    if self.busy > 0 || self.pending > 0 {
                        ui.spinner();
                        ui.label(RichText::new("sto lavorando...").weak());
                    }
                });
            });
            if self.client.is_none() {
                ui.label(RichText::new(self.steam_error.as_str()).color(RED).small());
            }
            if !self.shared_error.is_empty() {
                ui.label(
                    RichText::new(format!("⚠ Lista del gruppo non leggibile: {}", self.shared_error))
                        .color(ORANGE),
                );
            }
            if self.cfg.configured() {
                if self.cfg.mode == Mode::File && !looks_synced(&self.cfg.shared_path) {
                    ui.label(
                        RichText::new("⚠ Il file non sembra in una cartella che si sincronizza (Dropbox, Drive, Syncthing...): se è solo sul tuo PC gli altri non vedranno le modifiche.")
                            .color(ORANGE)
                            .small(),
                    );
                }
                if let Some(h) = self.shared.history.last() {
                    ui.label(
                        RichText::new(format!(
                            "Ultima modifica del gruppo: {} {} ha {} «{}»",
                            ago(h.ts),
                            h.by,
                            if h.added { "aggiunto" } else { "tolto" },
                            h.title
                        ))
                        .weak()
                        .small(),
                    );
                }
            }
            ui.add_space(4.0);
        });
    }

    fn toolbar_panel(&mut self, ctx: &egui::Context, rows: &[Row], actions: &mut Vec<Action>) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            let todo = rows.iter().filter(|r| Filter::Todo.matches(r)).count();
            let group = self.group_set();
            let group_size: u64 = group.iter().map(|id| self.size_of(id)).sum();

            ui.horizontal(|ui| {
                let label = if todo > 0 {
                    format!("Sincronizza con il gruppo ({todo} da sistemare)")
                } else {
                    "Sincronizza con il gruppo".to_string()
                };
                if ui
                    .add(egui::Button::new(RichText::new(label).strong()))
                    .on_hover_text("Rilegge la lista del gruppo e ti mostra cosa verrebbe aggiunto o tolto alle tue iscrizioni; applichi solo se confermi")
                    .clicked()
                {
                    actions.push(Action::Sync);
                }
                if ui
                    .button("Aggiorna")
                    .on_hover_text("Rilegge le tue iscrizioni da Steam e la lista del gruppo")
                    .clicked()
                {
                    actions.push(Action::Refresh);
                }
                if ui
                    .button("Storico")
                    .on_hover_text("Chi ha aggiunto o tolto cosa, e quando")
                    .clicked()
                {
                    self.show_history = !self.show_history;
                }
                ui.separator();
                ui.label(format!(
                    "Tue iscrizioni: {}   ·   Addon del gruppo: {}{}",
                    self.local.len(),
                    group.len(),
                    if group_size > 0 {
                        format!(" ({})", human_size(group_size))
                    } else {
                        String::new()
                    }
                ));
            });

            ui.horizontal(|ui| {
                ui.label("Nuovo addon o collezione:");
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.add_input)
                        .desired_width(320.0)
                        .hint_text("incolla il link o l'ID del Workshop"),
                );
                let has = !self.add_input.trim().is_empty();
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui
                    .add_enabled(has, egui::Button::new("Aggiungi per tutti"))
                    .on_hover_text("Lo mette nella lista del gruppo insieme agli oggetti che richiede e te lo iscrive. Se è una collezione, aggiunge tutti i suoi addon")
                    .clicked()
                    || (has && enter)
                {
                    actions.push(Action::AddIds);
                }
            });

            ui.horizontal(|ui| {
                ui.label("Mostra:");
                egui::ComboBox::from_id_salt("filtro")
                    .width(230.0)
                    .selected_text(self.filter.label())
                    .show_ui(ui, |ui| {
                        for f in Filter::ALL {
                            let n = rows.iter().filter(|r| f.matches(r)).count();
                            ui.selectable_value(&mut self.filter, f, format!("{} ({n})", f.label()));
                        }
                    });
                ui.label("Ordina per:");
                egui::ComboBox::from_id_salt("ordine")
                    .width(200.0)
                    .selected_text(self.sort.label())
                    .show_ui(ui, |ui| {
                        for s in SortBy::ALL {
                            ui.selectable_value(&mut self.sort, s, s.label());
                        }
                    });
                ui.label("Cerca:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("titolo o ID")
                        .desired_width(160.0),
                );
            });

            ui.separator();
            ui.horizontal(|ui| {
                ui.set_min_height(22.0);
                if self.selected.is_empty() {
                    ui.label(
                        RichText::new("Spunta uno o più addon nella lista per condividerli, toglierli a tutti o tenerli solo per te. Clicca un titolo per l'anteprima.")
                            .weak(),
                    );
                } else {
                    let n = self.selected.len();
                    let size: u64 = self.selected.iter().map(|id| self.size_of(id)).sum();
                    ui.label(RichText::new(format!("{n} spuntati {}", human_size(size))).strong());
                    if ui
                        .button("Condividi con il gruppo")
                        .on_hover_text("Li mette nella lista del gruppo, con gli oggetti che richiedono")
                        .clicked()
                    {
                        actions.push(Action::ShareSelected);
                    }
                    if ui
                        .button(RichText::new("Togli a tutto il gruppo").color(RED))
                        .on_hover_text("Li toglie dalla lista del gruppo: chi sincronizza li perderà. Chiede conferma")
                        .clicked()
                    {
                        actions.push(Action::AskRemove);
                    }
                    let all_ignored = self.selected.iter().all(|id| self.cfg.ignored.contains(id));
                    let ignore_label = if all_ignored {
                        "Rimetti nella sincronizzazione"
                    } else {
                        "Tieni solo per me"
                    };
                    if ui
                        .button(ignore_label)
                        .on_hover_text("La sincronizzazione non li aggiunge e non li toglie")
                        .clicked()
                    {
                        actions.push(Action::ToggleIgnore);
                    }
                    if ui.button("Togli le spunte").clicked() {
                        self.selected.clear();
                    }
                }
            });
            ui.add_space(2.0);
        });
    }

    fn log_panel(&self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .min_height(50.0)
            .default_height(80.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.log.is_empty() {
                            ui.label(
                                RichText::new("Qui compaiono le operazioni fatte.")
                                    .weak()
                                    .small(),
                            );
                        }
                        for l in &self.log {
                            ui.label(RichText::new(l.as_str()).monospace().small());
                        }
                    });
            });
    }

    fn preview_panel(&mut self, ctx: &egui::Context) {
        let Some(id) = self.current.clone() else {
            return;
        };
        let meta = self.meta.get(&id).cloned();
        let title = self.title_of(&id);
        let entry = self.shared.items.get(&id).cloned();
        let local = self.local_set();
        let group = self.group_set();
        let deps: Vec<(String, bool)> = meta
            .as_ref()
            .map(|m| {
                m.deps
                    .iter()
                    .map(|d| (self.title_of(d), local.contains(d) || group.contains(d)))
                    .collect()
            })
            .unwrap_or_default();
        egui::SidePanel::right("preview")
            .resizable(true)
            .default_width(300.0)
            .min_width(220.0)
            .max_width(500.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.strong("Anteprima");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Chiudi").clicked() {
                            self.current = None;
                        }
                    });
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.label(RichText::new(title.as_str()).heading());
                        ui.add_space(4.0);
                        let Some(m) = &meta else {
                            ui.label("Sto scaricando le informazioni...");
                            return;
                        };
                        if !m.preview.is_empty() {
                            let w = ui.available_width();
                            ui.add(
                                egui::Image::new(m.preview.as_str())
                                    .max_width(w)
                                    .max_height(220.0),
                            );
                            ui.add_space(4.0);
                        }
                        let warn = m.warning();
                        if !warn.is_empty() {
                            ui.label(RichText::new(format!("⚠ {warn}")).color(ORANGE));
                        }
                        let mut info = Vec::new();
                        if m.size > 0 {
                            info.push(format!("Peso: {}", human_size(m.size)));
                        }
                        if m.updated > 0 {
                            info.push(format!("aggiornato {}", ago(m.updated)));
                        }
                        if !info.is_empty() {
                            ui.label(info.join("   ·   "));
                        }
                        if let Some(e) = &entry {
                            if !e.by.is_empty() {
                                let what = if e.present { "Aggiunto" } else { "Tolto" };
                                ui.label(format!("{what} da {} {}", e.by, ago(e.ts)));
                            }
                        }
                        if !deps.is_empty() {
                            ui.add_space(4.0);
                            ui.strong(format!("Oggetti richiesti ({}):", deps.len()));
                            for (t, ok) in &deps {
                                if *ok {
                                    ui.label(format!("✔ {t}"));
                                } else {
                                    ui.label(RichText::new(format!("✖ {t} (manca)")).color(ORANGE));
                                }
                            }
                        }
                        ui.add_space(6.0);
                        if m.description.is_empty() {
                            ui.label(RichText::new("Nessuna descrizione.").weak());
                        } else {
                            ui.label(m.description.as_str());
                        }
                        ui.add_space(6.0);
                        ui.hyperlink_to(
                            "Apri nel Workshop",
                            format!("https://steamcommunity.com/sharedfiles/filedetails/?id={id}"),
                        );
                        ui.label(RichText::new(format!("ID {id}")).weak().small());
                    });
            });
    }

    /// Prima volta: bisogna scegliere dove tenere la lista del gruppo.
    fn welcome_panel(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.heading("Dove tenete la lista degli addon del gruppo?");
                ui.add_space(6.0);
                ui.label("Tutti nel gruppo devono scegliere lo stesso posto. Si può cambiare dopo da Impostazioni.");
                ui.add_space(16.0);
                let big = egui::vec2(420.0, 34.0);
                for (mode, desc) in [
                    (
                        Mode::Steam,
                        "Una collezione del Workshop: Steam la tiene aggiornata per tutti, niente da installare.",
                    ),
                    (
                        Mode::File,
                        "Un file in una cartella che già condividete (Dropbox, Google Drive, Syncthing...).",
                    ),
                    (
                        Mode::GitHub,
                        "Un file in un repository GitHub: serve un account GitHub e un token per ognuno.",
                    ),
                ] {
                    if ui
                        .add(egui::Button::new(RichText::new(mode.label()).strong()).min_size(big))
                        .clicked()
                    {
                        actions.push(Action::OpenSettings(mode));
                    }
                    ui.label(RichText::new(desc).weak());
                    ui.add_space(10.0);
                }
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("Intanto vedo {} tue iscrizioni su Steam.", self.local.len()))
                        .weak(),
                );
            });
        });
    }

    fn list_panel(&mut self, ctx: &egui::Context, rows: &[Row]) {
        egui::CentralPanel::default().show(ctx, |ui| {
            if !self.loaded_once && self.shared_error.is_empty() {
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Leggo la lista del gruppo...");
                });
                return;
            }
            let q = self.search.to_lowercase();
            let filter = self.filter;
            let visible: Vec<&Row> = rows
                .iter()
                .filter(|r| {
                    filter.matches(r)
                        && (q.is_empty()
                            || r.title.to_lowercase().contains(&q)
                            || r.id.contains(&q))
                })
                .collect();

            if visible.is_empty() {
                ui.add_space(12.0);
                if rows.is_empty() {
                    ui.label("Nessun addon: aggiungine uno con il campo qui sopra.");
                } else {
                    ui.label("Nessun addon corrisponde al filtro o alla ricerca.");
                }
                return;
            }

            let all_sel = visible.iter().all(|r| self.selected.contains(&r.id));
            let height = ui.available_height();
            TableBuilder::new(ui)
                .striped(true)
                .resizable(false)
                .auto_shrink([false, false])
                .max_scroll_height(height)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(26.0))
                .column(Column::exact(115.0))
                .column(Column::remainder().at_least(120.0).clip(true))
                .column(Column::exact(70.0))
                .column(Column::initial(200.0).at_least(60.0).clip(true))
                .header(22.0, |mut h| {
                    h.col(|ui| {
                        let mut v = all_sel;
                        if ui
                            .checkbox(&mut v, "")
                            .on_hover_text("Spunta o togli tutti quelli mostrati")
                            .changed()
                        {
                            for r in &visible {
                                if v {
                                    self.selected.insert(r.id.clone());
                                } else {
                                    self.selected.remove(&r.id);
                                }
                            }
                        }
                    });
                    h.col(|ui| {
                        ui.strong("Stato");
                    });
                    h.col(|ui| {
                        ui.strong(format!("Addon ({})", visible.len()));
                    });
                    h.col(|ui| {
                        ui.strong("Peso");
                    });
                    h.col(|ui| {
                        ui.strong("Avvisi");
                    });
                })
                .body(|body| {
                    body.rows(24.0, visible.len(), |mut row| {
                        let r = visible[row.index()];
                        row.col(|ui| {
                            let mut sel = self.selected.contains(&r.id);
                            if ui.checkbox(&mut sel, "").changed() {
                                if sel {
                                    self.selected.insert(r.id.clone());
                                } else {
                                    self.selected.remove(&r.id);
                                }
                            }
                        });
                        row.col(|ui| {
                            let (txt, col) = status_style(r.status);
                            ui.label(RichText::new(txt).color(col));
                        });
                        row.col(|ui| {
                            let is_current = self.current.as_deref() == Some(r.id.as_str());
                            if ui
                                .selectable_label(is_current, r.title.as_str())
                                .on_hover_text("Clicca per l'anteprima")
                                .clicked()
                            {
                                self.current = if is_current { None } else { Some(r.id.clone()) };
                            }
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(human_size(r.size)).weak());
                        });
                        row.col(|ui| {
                            if !r.warn.is_empty() {
                                ui.label(RichText::new(r.warn.as_str()).color(ORANGE))
                                    .on_hover_text(r.warn.as_str());
                            }
                        });
                    });
                });
        });
    }

    fn settings_window(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(d) = self.settings.as_mut() else {
            return;
        };
        let mut open = true;
        let mut save = false;
        egui::Window::new("Impostazioni")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.strong("Dove sta la lista del gruppo");
                ui.horizontal(|ui| {
                    for m in [Mode::Steam, Mode::File, Mode::GitHub] {
                        ui.selectable_value(&mut d.mode, m, m.label());
                    }
                });
                ui.add_space(6.0);
                match d.mode {
                    Mode::Steam => {
                        ui.label("Uno del gruppo crea una collezione del Workshop di Garry's Mod, ci mette almeno un addon e la pubblica (visibilità «Non in elenco» o «Solo amici»). Poi, nella pagina della collezione, aggiunge gli altri come collaboratori. Ognuno incolla qui lo stesso link.");
                        ui.hyperlink_to(
                            "Apri le tue collezioni di Garry's Mod su Steam",
                            "https://steamcommunity.com/my/myworkshopfiles/?section=collections&appid=4000",
                        );
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label("Link della collezione:");
                            ui.add(
                                egui::TextEdit::singleline(&mut d.collection)
                                    .desired_width(330.0)
                                    .hint_text("https://steamcommunity.com/sharedfiles/filedetails/?id=..."),
                            );
                        });
                        ui.label(RichText::new("Nota: lo storico qui sa chi ha fatto cosa solo per le modifiche fatte da questo PC; le altre compaiono come «qualcuno».").weak().small());
                    }
                    Mode::File => {
                        ui.label("Il file deve stare in una cartella che si sincronizza da sola tra i vostri PC (Dropbox, Google Drive, OneDrive, Syncthing...). Il primo lo crea, gli altri scelgono lo stesso file nella loro copia della cartella.");
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button("Crea nuovo file…").clicked() {
                                let mut dlg = rfd::FileDialog::new()
                                    .set_title("Dove salvo il file del gruppo? (in una cartella condivisa)")
                                    .set_file_name("gmod-workshop-sync.json")
                                    .add_filter("File JSON", &["json"]);
                                if let Some(h) = dirs::home_dir() {
                                    dlg = dlg.set_directory(h);
                                }
                                if let Some(p) = dlg.save_file() {
                                    d.path = p.display().to_string();
                                }
                            }
                            if ui.button("Scegli file esistente…").clicked() {
                                let mut dlg = rfd::FileDialog::new()
                                    .set_title("Scegli il file del gruppo")
                                    .add_filter("File JSON", &["json"]);
                                if let Some(h) = dirs::home_dir() {
                                    dlg = dlg.set_directory(h);
                                }
                                if let Some(p) = dlg.pick_file() {
                                    d.path = p.display().to_string();
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("File:");
                            ui.add(
                                egui::TextEdit::singleline(&mut d.path)
                                    .desired_width(420.0)
                                    .hint_text("nessun file scelto"),
                            );
                        });
                        if !d.path.trim().is_empty() && !looks_synced(&d.path) {
                            ui.label(
                                RichText::new("⚠ Questa cartella non sembra sincronizzata: se è solo sul tuo PC, gli altri non vedranno niente.")
                                    .color(ORANGE)
                                    .small(),
                            );
                        }
                    }
                    Mode::GitHub => {
                        ui.label("Uno crea un repository GitHub (anche privato) e aggiunge gli altri come collaboratori. Ognuno crea un token «fine-grained» con permesso Contents: Read and write su quel repository e lo incolla qui.");
                        ui.hyperlink_to(
                            "Crea un token su GitHub",
                            "https://github.com/settings/personal-access-tokens/new",
                        );
                        ui.add_space(4.0);
                        egui::Grid::new("gh").num_columns(2).show(ui, |ui| {
                            ui.label("Repository:");
                            ui.add(
                                egui::TextEdit::singleline(&mut d.gh_repo)
                                    .desired_width(300.0)
                                    .hint_text("utente/nome-repo"),
                            );
                            ui.end_row();
                            ui.label("File nel repository:");
                            ui.add(egui::TextEdit::singleline(&mut d.gh_path).desired_width(300.0));
                            ui.end_row();
                            ui.label("Token:");
                            ui.add(
                                egui::TextEdit::singleline(&mut d.gh_token)
                                    .password(true)
                                    .desired_width(300.0),
                            );
                            ui.end_row();
                        });
                        ui.label(RichText::new("Il token resta salvato solo su questo PC, nelle impostazioni del programma.").weak().small());
                    }
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Il tuo nome:");
                    ui.add(
                        egui::TextEdit::singleline(&mut d.nick)
                            .desired_width(160.0)
                            .hint_text(default_nick()),
                    );
                    ui.label(RichText::new("compare nello storico").weak().small());
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Salva").strong()).clicked() {
                        save = true;
                    }
                });
            });
        if save {
            actions.push(Action::SaveSettings);
        } else if !open {
            self.settings = None;
        }
    }

    fn plan_window(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(plan) = self.plan.as_mut() else {
            return;
        };
        egui::Window::new("Cosa cambierà")
            .collapsible(false)
            .resizable(true)
            .default_width(520.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                if plan.subscribe.is_empty() && plan.unsubscribe.is_empty() {
                    ui.label("Sei già allineato con il gruppo: niente da fare.");
                }
                egui::ScrollArea::vertical()
                    .max_height(340.0)
                    .show(ui, |ui| {
                        if !plan.subscribe.is_empty() {
                            ui.strong(format!(
                                "Da aggiungere alle tue iscrizioni ({})",
                                plan.subscribe.len()
                            ));
                            for (_, title, size, on) in plan.subscribe.iter_mut() {
                                ui.checkbox(on, format!("{title}   {}", human_size(*size)));
                            }
                        }
                        if !plan.unsubscribe.is_empty() {
                            ui.add_space(6.0);
                            if plan.unsubscribe.len() > 10 {
                                ui.label(
                                    RichText::new("⚠ Sono tanti: controlla che sia davvero quello che il gruppo vuole prima di applicare.")
                                        .color(RED),
                                );
                            }
                            ui.strong(format!(
                                "Da togliere dalle tue iscrizioni ({})",
                                plan.unsubscribe.len()
                            ));
                            for (_, title, _, on) in plan.unsubscribe.iter_mut() {
                                ui.checkbox(on, title.as_str());
                            }
                        }
                    });
                ui.separator();
                let n = plan.subscribe.iter().filter(|x| x.3).count()
                    + plan.unsubscribe.iter().filter(|x| x.3).count();
                let download: u64 = plan.subscribe.iter().filter(|x| x.3).map(|x| x.2).sum();
                if download > 0 {
                    ui.label(format!("Da scaricare: {}", human_size(download)));
                }
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(n > 0, egui::Button::new(format!("Applica ({n})")))
                        .clicked()
                    {
                        actions.push(Action::ApplyPlan);
                    }
                    if ui.button("Chiudi").clicked() {
                        actions.push(Action::ClosePlan);
                    }
                });
                ui.label(
                    RichText::new("Meglio con Garry's Mod chiuso. Prima di applicare salvo un backup delle tue iscrizioni.")
                        .small()
                        .weak(),
                );
            });
    }

    fn confirm_window(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(ids) = self.confirm_remove.clone() else {
            return;
        };
        let titles: Vec<String> = ids.iter().take(8).map(|i| self.title_of(i)).collect();
        egui::Window::new("Togliere a tutto il gruppo?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(format!(
                    "{} addon verranno tolti dalla lista del gruppo: chi sincronizza dopo li perderà.",
                    ids.len()
                ));
                for t in &titles {
                    ui.label(format!("• {t}"));
                }
                if ids.len() > titles.len() {
                    ui.label(format!("… e altri {}", ids.len() - titles.len()));
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(RichText::new("Sì, togli a tutti").color(RED))
                        .clicked()
                    {
                        actions.push(Action::ConfirmRemove);
                    }
                    if ui.button("Annulla").clicked() {
                        actions.push(Action::CancelRemove);
                    }
                });
            });
    }

    fn history_window(&mut self, ctx: &egui::Context) {
        if !self.show_history {
            return;
        }
        let mut open = true;
        let entries: Vec<HistoryEntry> = self
            .shared
            .history
            .iter()
            .rev()
            .take(300)
            .cloned()
            .collect();
        egui::Window::new("Storico del gruppo")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(480.0)
            .default_height(380.0)
            .show(ctx, |ui| {
                if entries.is_empty() {
                    ui.label("Ancora nessuna modifica registrata.");
                    return;
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for e in &entries {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new(ago(e.ts)).weak());
                                ui.label(RichText::new(e.by.as_str()).strong());
                                if e.added {
                                    ui.label(RichText::new("ha aggiunto").color(GREEN));
                                } else {
                                    ui.label(RichText::new("ha tolto").color(RED));
                                }
                                ui.label(e.title.as_str());
                            });
                        }
                    });
            });
        if !open {
            self.show_history = false;
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump(ctx);
        let rows = self.build_rows();
        let mut actions: Vec<Action> = Vec::new();
        self.header_panel(ctx, &mut actions);
        if !self.cfg.configured() {
            self.log_panel(ctx);
            self.welcome_panel(ctx, &mut actions);
        } else {
            self.toolbar_panel(ctx, &rows, &mut actions);
            self.log_panel(ctx);
            self.preview_panel(ctx);
            self.list_panel(ctx, &rows);
            self.plan_window(ctx, &mut actions);
            self.confirm_window(ctx, &mut actions);
            self.history_window(ctx);
        }
        self.settings_window(ctx, &mut actions);
        for a in actions {
            self.run(a);
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1150.0, 720.0])
            .with_min_inner_size([820.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Garry's Mod Workshop Sync",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
