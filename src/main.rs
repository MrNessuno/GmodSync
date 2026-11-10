// Garry's Mod Workshop Sync
// Tiene allineate le iscrizioni al Workshop di un gruppo di amici.
// Richiede Steam aperto, con l'account che possiede Garry's Mod.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use steamworks::{AppId, Client, PublishedFileId};

const GMOD_APP_ID: u32 = 4000;
const GREEN: Color32 = Color32::from_rgb(60, 160, 80);
const ORANGE: Color32 = Color32::from_rgb(230, 150, 40);
const RED: Color32 = Color32::from_rgb(210, 70, 70);
const BLUE: Color32 = Color32::from_rgb(70, 140, 220);
const HISTORY_MAX: usize = 1000;
const API: &str = "https://api.steampowered.com/ISteamRemoteStorage";

// ---------------------------------------------------------------------
// Stato condiviso del gruppo
// ---------------------------------------------------------------------

/// Una riga del file condiviso: present = true vuol dire "tutti devono averlo",
/// present = false vuol dire "tutti devono toglierlo".
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

/// Dove vive lo stato condiviso. Per ora esiste solo il file; in futuro
/// si possono aggiungere altri modi implementando questo trait.
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
        if text.trim().is_empty() {
            return Ok(Shared::default());
        }
        serde_json::from_str(&text).map_err(|e| format!("file del gruppo non valido: {e}"))
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

/// Rilegge il file, applica la modifica e lo riscrive, così si perdono meno
/// modifiche fatte da altri nel frattempo.
fn mutate(backend: &dyn Backend, f: impl FnOnce(&mut Shared)) -> Result<Shared, String> {
    let mut shared = backend.load()?;
    f(&mut shared);
    backend.save(&shared)?;
    Ok(shared)
}

// ---------------------------------------------------------------------
// Impostazioni locali
// ---------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Default)]
struct Config {
    #[serde(default)]
    shared_path: String,
    #[serde(default)]
    nickname: String,
    #[serde(default)]
    ignored: BTreeSet<String>,
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

// ---------------------------------------------------------------------
// Informazioni sugli oggetti (API web pubblica di Steam, senza chiave)
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
    /// Addon richiesti da questo addon (dipendenze)
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

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build()
}

fn num(v: &serde_json::Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

/// Figli di ogni ID: per una collezione sono i suoi addon, per un addon
/// normale sono gli addon che richiede. Ogni figlio è (id, tipo): tipo 0 = addon, 2 = collezione.
fn fetch_children(ids: &[String]) -> Result<BTreeMap<String, Vec<(String, u64)>>, String> {
    let agent = agent();
    let mut out = BTreeMap::new();
    for chunk in ids.chunks(100) {
        let mut form: Vec<(String, String)> =
            vec![("collectioncount".to_string(), chunk.len().to_string())];
        for (i, id) in chunk.iter().enumerate() {
            form.push((format!("publishedfileids[{i}]"), id.clone()));
        }
        let pairs: Vec<(&str, &str)> = form.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let resp: serde_json::Value = agent
            .post(&format!("{API}/GetCollectionDetails/v1/"))
            .send_form(&pairs)
            .map_err(|e| format!("rete: {e}"))?
            .into_json()
            .map_err(|e| format!("risposta non valida: {e}"))?;
        if let Some(list) = resp["response"]["collectiondetails"].as_array() {
            for d in list {
                let id = d["publishedfileid"].as_str().unwrap_or("").to_string();
                if id.is_empty() {
                    continue;
                }
                let mut kids = Vec::new();
                if let Some(ch) = d["children"].as_array() {
                    for c in ch {
                        let cid = c["publishedfileid"].as_str().unwrap_or("").to_string();
                        if !cid.is_empty() && cid != id {
                            kids.push((cid, num(&c["filetype"])));
                        }
                    }
                }
                out.insert(id, kids);
            }
        }
    }
    Ok(out)
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
            .post(&format!("{API}/GetPublishedFileDetails/v1/"))
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
    // Le dipendenze sono un extra: se la richiesta fallisce, pazienza.
    if let Ok(children) = fetch_children(ids) {
        for (id, kids) in children {
            if let Some(m) = out.get_mut(&id) {
                m.deps = kids
                    .into_iter()
                    .filter(|(_, t)| *t == 0)
                    .map(|(c, _)| c)
                    .collect();
            }
        }
    }
    Ok(out)
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
    ImportCollection,
    BrowseFile,
    NewFile,
}

/// Esito di una iscrizione/disiscrizione, che Steam comunica in modo asincrono.
struct Done {
    id: u64,
    what: &'static str,
    result: Result<(), String>,
}

// ---------------------------------------------------------------------
// Applicazione
// ---------------------------------------------------------------------

struct App {
    ctx: egui::Context,
    client: Option<Client>,
    steam_error: String,
    cfg: Config,
    nick_input: String,
    shared: Shared,
    shared_error: String,
    local: Vec<u64>,
    meta: BTreeMap<String, Meta>,
    meta_rx: Option<Receiver<Result<BTreeMap<String, Meta>, String>>>,
    steam_tx: Sender<Done>,
    steam_rx: Receiver<Done>,
    pending: usize,
    selected: BTreeSet<String>,
    current: Option<String>,
    search: String,
    filter: Filter,
    sort: SortBy,
    add_input: String,
    plan: Option<Plan>,
    confirm_remove: Option<Vec<String>>,
    show_history: bool,
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
        let (steam_tx, steam_rx) = channel();
        let nick_input = cfg.nickname.clone();
        let mut app = App {
            ctx: cc.egui_ctx.clone(),
            client,
            steam_error,
            cfg,
            nick_input,
            shared: Shared::default(),
            shared_error: String::new(),
            local: Vec::new(),
            meta: BTreeMap::new(),
            meta_rx: None,
            steam_tx,
            steam_rx,
            pending: 0,
            selected: BTreeSet::new(),
            current: None,
            search: String::new(),
            filter: Filter::All,
            sort: SortBy::Status,
            add_input: String::new(),
            plan: None,
            confirm_remove: None,
            show_history: false,
            log: Vec::new(),
        };
        app.reload_all();
        app
    }

    // ----- dati -----

    fn backend(&self) -> Option<FileBackend> {
        let p = self.cfg.shared_path.trim();
        if p.is_empty() {
            None
        } else {
            Some(FileBackend {
                path: PathBuf::from(p),
            })
        }
    }

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

    fn reload_all(&mut self) {
        self.refresh_local();
        self.reload_shared();
        self.start_meta();
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
        match self.backend() {
            None => {
                self.shared = Shared::default();
                self.shared_error.clear();
            }
            Some(b) => match b.load() {
                Ok(s) => {
                    self.shared = s;
                    self.shared_error.clear();
                }
                Err(e) => self.shared_error = e,
            },
        }
    }

    fn start_meta(&mut self) {
        let mut ids = self.local_set();
        ids.extend(self.group_set());
        let ids: Vec<String> = ids.into_iter().collect();
        if ids.is_empty() {
            return;
        }
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let r = fetch_meta(&ids);
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        self.meta_rx = Some(rx);
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
            if warn.is_empty() && group.contains(&id) {
                if let Some(m) = meta {
                    let miss = m
                        .deps
                        .iter()
                        .filter(|d| !group.contains(*d) && !local.contains(*d))
                        .count();
                    if miss > 0 {
                        warn = format!("richiede {miss} addon che il gruppo non ha");
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

    /// Riceve gli esiti di Steam e le informazioni scaricate.
    fn pump(&mut self, ctx: &egui::Context) {
        if let Some(c) = &self.client {
            c.run_callbacks();
        }

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

        let mut got = None;
        if let Some(rx) = &self.meta_rx {
            if let Ok(r) = rx.try_recv() {
                got = Some(r);
            }
        }
        if let Some(r) = got {
            self.meta_rx = None;
            match r {
                Ok(m) => self.meta.extend(m),
                Err(e) => self
                    .log
                    .push(format!("Informazioni sugli addon non scaricate: {e}")),
            }
        }

        if self.pending > 0 || self.meta_rx.is_some() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    // ----- file del gruppo -----

    fn publish(&mut self, ids: &[String], present: bool) -> bool {
        let Some(b) = self.backend() else {
            self.log
                .push("Scegli prima il file del gruppo.".to_string());
            return false;
        };
        let by = self.nick();
        let items: Vec<(String, String)> =
            ids.iter().map(|i| (i.clone(), self.title_of(i))).collect();
        let res = mutate(&b, |s| {
            let t = now();
            for (id, title) in items {
                s.history.push(HistoryEntry {
                    ts: t,
                    by: by.clone(),
                    added: present,
                    id: id.clone(),
                    title: title.clone(),
                });
                s.items.insert(
                    id,
                    Entry {
                        present,
                        title,
                        by: by.clone(),
                        ts: t,
                    },
                );
            }
            if s.history.len() > HISTORY_MAX {
                let extra = s.history.len() - HISTORY_MAX;
                s.history.drain(0..extra);
            }
        });
        match res {
            Ok(s) => {
                self.shared = s;
                true
            }
            Err(e) => {
                self.log
                    .push(format!("Salvataggio nel file del gruppo fallito: {e}"));
                false
            }
        }
    }

    /// Aggiunge alla lista le dipendenze (anche indirette) degli addon dati.
    fn expand_deps(&mut self, ids: Vec<String>) -> Vec<String> {
        let mut all = ids.clone();
        let mut seen: BTreeSet<String> = ids.iter().cloned().collect();
        let mut frontier = ids;
        let mut added: Vec<(String, String)> = Vec::new(); // (dipendenza, richiesta da)
        for _ in 0..5 {
            let unknown: Vec<String> = frontier
                .iter()
                .filter(|i| !self.meta.contains_key(*i))
                .cloned()
                .collect();
            if !unknown.is_empty() {
                match fetch_meta(&unknown) {
                    Ok(m) => self.meta.extend(m),
                    Err(e) => self.log.push(format!("Controllo addon non riuscito: {e}")),
                }
            }
            let mut next = Vec::new();
            for id in &frontier {
                let deps = self
                    .meta
                    .get(id)
                    .map(|m| m.deps.clone())
                    .unwrap_or_default();
                for d in deps {
                    if seen.insert(d.clone()) {
                        next.push(d.clone());
                        all.push(d.clone());
                        added.push((d, id.clone()));
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        let unknown: Vec<String> = all
            .iter()
            .filter(|i| !self.meta.contains_key(*i))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            if let Ok(m) = fetch_meta(&unknown) {
                self.meta.extend(m);
            }
        }
        for (d, parent) in added {
            let line = format!(
                "Aggiunta dipendenza: {} (serve a {})",
                self.title_of(&d),
                self.title_of(&parent)
            );
            self.log.push(line);
        }
        all
    }

    /// Condivide gli addon con il gruppo (con le loro dipendenze) e li iscrive qui.
    fn share(&mut self, ids: Vec<String>) {
        if self.backend().is_none() {
            self.log
                .push("Scegli prima il file del gruppo.".to_string());
            return;
        }
        let local = self.local_set();
        let all = self.expand_deps(ids);
        let mut good: Vec<String> = Vec::new();
        for id in all {
            match self.meta.get(&id) {
                Some(m) if m.deleted() || m.wrong_app() => {
                    let line = format!("Scartato {}: {}", self.title_of(&id), m.warning());
                    self.log.push(line);
                }
                None if !local.contains(&id) => {
                    self.log.push(format!("Scartato {id}: ID sconosciuto"));
                }
                _ => good.push(id),
            }
        }
        if good.is_empty() {
            return;
        }
        if self.publish(&good, true) {
            for id in &good {
                self.cfg.ignored.remove(id);
                if !local.contains(id) {
                    self.subscribe(id);
                }
            }
            self.cfg.save();
            self.log.push(format!(
                "Condivisi con il gruppo: {} addon ({})",
                good.len(),
                human_size(good.iter().map(|i| self.size_of(i)).sum())
            ));
        }
    }

    fn import_collection(&mut self) {
        let ids = parse_ids(&self.add_input);
        if ids.is_empty() {
            self.log
                .push("Incolla il link o l'ID della collezione.".to_string());
            return;
        }
        let mut items: Vec<String> = Vec::new();
        let mut cols = ids;
        for _ in 0..3 {
            let children = match fetch_children(&cols) {
                Ok(c) => c,
                Err(e) => {
                    self.log
                        .push(format!("Lettura della collezione non riuscita: {e}"));
                    return;
                }
            };
            let mut sub = Vec::new();
            for (_, kids) in children {
                for (cid, kind) in kids {
                    if kind == 2 {
                        sub.push(cid);
                    } else if !items.contains(&cid) {
                        items.push(cid);
                    }
                }
            }
            if sub.is_empty() {
                break;
            }
            cols = sub;
        }
        if items.is_empty() {
            self.log
                .push("Nessun addon trovato: il link è di una collezione pubblica?".to_string());
            return;
        }
        self.log
            .push(format!("Collezione letta: {} addon", items.len()));
        self.share(items);
        self.add_input.clear();
    }

    fn set_path(&mut self, p: PathBuf) {
        self.cfg.shared_path = p.display().to_string();
        self.cfg.save();
        let b = FileBackend { path: p };
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
        self.reload_all();
    }

    // ----- azioni -----

    fn run(&mut self, a: Action) {
        match a {
            Action::Refresh => {
                self.reload_all();
                self.log.push("Lista aggiornata.".to_string());
            }
            Action::Sync => {
                self.reload_all();
                self.plan = Some(self.make_plan());
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
                    self.share(ids);
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
                    if self.publish(&ids, false) {
                        self.backup();
                        let local = self.local_set();
                        for id in &ids {
                            self.cfg.ignored.remove(id);
                            if local.contains(id) {
                                self.unsubscribe(id);
                            }
                        }
                        self.cfg.save();
                        self.log
                            .push(format!("Tolti a tutto il gruppo: {}", ids.len()));
                        self.selected.clear();
                    }
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
                    self.share(ids);
                    self.add_input.clear();
                }
            }
            Action::ImportCollection => self.import_collection(),
            Action::BrowseFile => {
                let mut dlg = rfd::FileDialog::new()
                    .set_title("Scegli il file del gruppo")
                    .add_filter("File JSON", &["json"]);
                if let Some(h) = dirs::home_dir() {
                    dlg = dlg.set_directory(h);
                }
                if let Some(p) = dlg.pick_file() {
                    self.set_path(p);
                }
            }
            Action::NewFile => {
                let mut dlg = rfd::FileDialog::new()
                    .set_title("Dove salvo il file del gruppo? (meglio in una cartella condivisa)")
                    .set_file_name("gmod-workshop-sync.json")
                    .add_filter("File JSON", &["json"]);
                if let Some(h) = dirs::home_dir() {
                    dlg = dlg.set_directory(h);
                }
                if let Some(p) = dlg.save_file() {
                    self.set_path(p);
                }
            }
        }
    }

    // ----- disegno -----

    /// Riga in alto: stato di Steam, file del gruppo, nome.
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
                ui.separator();
                if let Some(b) = self.backend() {
                    let name = b
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    ui.label("File del gruppo:");
                    ui.label(RichText::new(name).strong())
                        .on_hover_text(b.path.display().to_string());
                    if ui
                        .small_button("Cambia…")
                        .on_hover_text("Scegli un altro file del gruppo")
                        .clicked()
                    {
                        actions.push(Action::BrowseFile);
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.nick_input)
                            .desired_width(120.0)
                            .hint_text(default_nick()),
                    );
                    if r.lost_focus() && self.nick_input.trim() != self.cfg.nickname {
                        self.cfg.nickname = self.nick_input.trim().to_string();
                        self.cfg.save();
                    }
                    ui.label("Il tuo nome:")
                        .on_hover_text("Compare nello storico accanto alle tue modifiche");
                });
            });
            if self.client.is_none() {
                ui.label(RichText::new(self.steam_error.as_str()).color(RED).small());
            }
            if !self.shared_error.is_empty() {
                ui.label(RichText::new(self.shared_error.as_str()).color(ORANGE));
            }
            ui.add_space(4.0);
        });
    }

    /// Strumenti: sincronizza, aggiungi, filtri, azioni sulle spunte.
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
                    .on_hover_text("Ti mostra cosa verrebbe aggiunto o tolto alle tue iscrizioni; applichi solo se confermi")
                    .clicked()
                {
                    actions.push(Action::Sync);
                }
                if ui
                    .button("Aggiorna")
                    .on_hover_text("Rilegge le tue iscrizioni da Steam e il file del gruppo")
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
                    "Tue iscrizioni: {}   ·   Addon del gruppo: {} ({})",
                    self.local.len(),
                    group.len(),
                    if group_size > 0 {
                        human_size(group_size)
                    } else {
                        "peso sconosciuto".to_string()
                    }
                ));
            });

            ui.horizontal(|ui| {
                ui.label("Nuovo addon o collezione:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.add_input)
                        .desired_width(300.0)
                        .hint_text("incolla il link o l'ID del Workshop"),
                );
                let has = !self.add_input.trim().is_empty();
                if ui
                    .add_enabled(has, egui::Button::new("Aggiungi per tutti"))
                    .on_hover_text("Lo dà a tutto il gruppo (con gli addon che richiede) e te lo iscrive subito")
                    .clicked()
                {
                    actions.push(Action::AddIds);
                }
                if ui
                    .add_enabled(has, egui::Button::new("Importa collezione"))
                    .on_hover_text("Il link è di una collezione: dà a tutto il gruppo tutti i suoi addon")
                    .clicked()
                {
                    actions.push(Action::ImportCollection);
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
                    ui.label(RichText::new(format!("{n} spuntati ({})", human_size(size))).strong());
                    if ui
                        .button("Condividi con il gruppo")
                        .on_hover_text("Li dà a tutti (con gli addon che richiedono)")
                        .clicked()
                    {
                        actions.push(Action::ShareSelected);
                    }
                    if ui
                        .button(RichText::new("Togli a tutto il gruppo").color(RED))
                        .on_hover_text("Chi sincronizza li perderà dalle sue iscrizioni. Chiede conferma")
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
            .default_height(70.0)
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
        let deps: Vec<(String, String, bool)> = meta
            .as_ref()
            .map(|m| {
                m.deps
                    .iter()
                    .map(|d| {
                        (
                            d.clone(),
                            self.title_of(d),
                            local.contains(d) || group.contains(d),
                        )
                    })
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
                            let what = if e.present { "Condiviso" } else { "Tolto" };
                            ui.label(format!("{what} da {} {}", e.by, ago(e.ts)));
                        }
                        if !deps.is_empty() {
                            ui.add_space(4.0);
                            ui.strong(format!("Richiede {} addon:", deps.len()));
                            for (_, t, ok) in &deps {
                                if *ok {
                                    ui.label(format!("• {t}"));
                                } else {
                                    ui.label(RichText::new(format!("• {t} (manca)")).color(ORANGE));
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

    /// Prima volta: niente file del gruppo, spieghiamo cosa fare.
    fn welcome_panel(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.heading("Per iniziare serve il file del gruppo");
                ui.add_space(8.0);
                ui.label("È un piccolo file dove il programma scrive quali addon deve avere il gruppo.");
                ui.label("Mettilo in una cartella che già condividete (Dropbox, Google Drive, Syncthing...).");
                ui.add_space(16.0);
                let big = egui::vec2(380.0, 36.0);
                if ui
                    .add(egui::Button::new("Sono il primo del gruppo: crea il file").min_size(big))
                    .clicked()
                {
                    actions.push(Action::NewFile);
                }
                ui.add_space(8.0);
                if ui
                    .add(egui::Button::new("Un amico l'ha già creato: sceglilo").min_size(big))
                    .clicked()
                {
                    actions.push(Action::BrowseFile);
                }
                ui.add_space(16.0);
                ui.label(
                    RichText::new(format!(
                        "Intanto vedo {} tue iscrizioni su Steam.",
                        self.local.len()
                    ))
                    .weak(),
                );
            });
        });
    }

    fn list_panel(&mut self, ctx: &egui::Context, rows: &[Row]) {
        egui::CentralPanel::default().show(ctx, |ui| {
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
                .column(Column::initial(190.0).at_least(60.0).clip(true))
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
                let download: u64 = plan
                    .subscribe
                    .iter()
                    .filter(|x| x.3)
                    .map(|x| x.2)
                    .sum();
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
                    "{} addon verranno tolti a tutti: chi sincronizza dopo di te li perderà.",
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
        if self.backend().is_none() {
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
