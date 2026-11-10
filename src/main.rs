// Garry's Mod Workshop Sync
// Tiene allineate le iscrizioni al Workshop di un gruppo di amici.
// Richiede Steam aperto, con l'account che possiede Garry's Mod.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, RichText};
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

// ---------------------------------------------------------------------
// Stato condiviso del gruppo
// ---------------------------------------------------------------------

/// Una riga del file condiviso: present = true vuol dire "tutti devono averlo",
/// present = false vuol dire "tutti devono toglierlo" (lapide).
#[derive(Serialize, Deserialize, Clone, Default)]
struct Entry {
    present: bool,
    title: String,
    by: String,
    ts: u64,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Shared {
    #[serde(default)]
    items: BTreeMap<String, Entry>,
}

/// Dove vive lo stato condiviso. Per ora esiste solo il file; in futuro
/// si possono aggiungere altri modi (Gist, server HTTP...) implementando questo trait.
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
        serde_json::from_str(&text).map_err(|e| format!("file condiviso non valido: {e}"))
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
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build();
    let mut out = BTreeMap::new();
    for chunk in ids.chunks(100) {
        let mut form: Vec<(String, String)> =
            vec![("itemcount".to_string(), chunk.len().to_string())];
        for (i, id) in chunk.iter().enumerate() {
            form.push((format!("publishedfileids[{i}]"), id.clone()));
        }
        let pairs: Vec<(&str, &str)> = form.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let resp: serde_json::Value = agent
            .post("https://api.steampowered.com/ISteamRemoteStorage/GetPublishedFileDetails/v1/")
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
                        ok: d["result"].as_u64() == Some(1),
                        app: d["consumer_app_id"].as_u64().unwrap_or(0),
                        vis: d["visibility"].as_u64().unwrap_or(0),
                        banned: d["banned"].as_u64() == Some(1),
                    },
                );
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Righe, filtri, piano di sincronizzazione, azioni
// ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Missing,   // lo ha il gruppo, tu no
    Remove,    // il gruppo l'ha tolto, tu ce l'hai ancora
    LocalOnly, // ce l'hai solo tu (non è nel file condiviso)
    Ok,
    Ignored,
}

fn status_style(s: Status) -> (&'static str, Color32) {
    match s {
        Status::Missing => ("Manca a te", ORANGE),
        Status::Remove => ("Tolto dal gruppo", RED),
        Status::LocalOnly => ("Solo tuo", BLUE),
        Status::Ok => ("A posto", GREEN),
        Status::Ignored => ("Solo per te", Color32::GRAY),
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
            Filter::All => "Tutti gli addon",
            Filter::Todo => "Da sistemare (manca, tolto o solo tuo)",
            Filter::LocalOnly => "Solo tuoi (non ancora condivisi)",
            Filter::Missing => "Mancano a te (li ha il gruppo)",
            Filter::Remove => "Da rimuovere (tolti dal gruppo)",
            Filter::Ok => "A posto",
            Filter::Ignored => "Tenuti solo per te",
            Filter::Problems => "Con problemi (eliminati, non pubblici...)",
            Filter::Deleted => "Eliminati o non trovati",
        }
    }

    fn matches(self, r: &Row) -> bool {
        match self {
            Filter::All => true,
            Filter::Todo => matches!(
                r.status,
                Status::Missing | Status::Remove | Status::LocalOnly
            ),
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

#[derive(Clone)]
struct Row {
    id: String,
    title: String,
    status: Status,
    warn: String,
    deleted: bool,
}

#[derive(Default)]
struct Plan {
    // (id, titolo, spuntato)
    subscribe: Vec<(String, String, bool)>,
    unsubscribe: Vec<(String, String, bool)>,
}

enum Action {
    Refresh,
    Sync,
    ApplyPlan,
    ClosePlan,
    AddSelected,
    AskRemove,
    ConfirmRemove,
    CancelRemove,
    ToggleIgnore,
    AddIds,
    SaveSettings,
    BrowseFile,
    NewFile,
}

/// Esito di una iscrizione/disiscrizione, che Steam ci comunica in modo asincrono.
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
    path_input: String,
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
    filter: String,
    filter_kind: Filter,
    add_input: String,
    plan: Option<Plan>,
    confirm_remove: Option<Vec<String>>,
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
                    "Steam non risponde: apri Steam, accedi con l'account che ha Garry's Mod e riavvia il programma ({e:?})"
                ),
            ),
        };
        let (steam_tx, steam_rx) = channel();
        let path_input = cfg.shared_path.clone();
        let nick_input = if cfg.nickname.is_empty() {
            default_nick()
        } else {
            cfg.nickname.clone()
        };
        let mut app = App {
            ctx: cc.egui_ctx.clone(),
            client,
            steam_error,
            cfg,
            path_input,
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
            filter: String::new(),
            filter_kind: Filter::All,
            add_input: String::new(),
            plan: None,
            confirm_remove: None,
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

    fn local_set(&self) -> BTreeSet<String> {
        self.local.iter().map(|i| i.to_string()).collect()
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
        let mut ids: BTreeSet<String> = self.local_set();
        for (id, e) in &self.shared.items {
            if e.present {
                ids.insert(id.clone());
            }
        }
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

    fn build_rows(&self) -> Vec<Row> {
        let local = self.local_set();
        let mut ids = local.clone();
        for (id, e) in &self.shared.items {
            if e.present {
                ids.insert(id.clone());
            }
        }
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
            rows.push(Row {
                title: self.title_of(&id),
                warn: meta.map(|m| m.warning()).unwrap_or_default(),
                deleted: meta.map(|m| m.deleted()).unwrap_or(false),
                status,
                id,
            });
        }
        rows.sort_by(|a, b| {
            a.status
                .cmp(&b.status)
                .then(a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });
        rows
    }

    fn make_plan(&self) -> Plan {
        let mut p = Plan::default();
        for r in self.build_rows() {
            match r.status {
                Status::Missing => p.subscribe.push((r.id, r.title, !r.deleted)),
                Status::Remove => p.unsubscribe.push((r.id, r.title, true)),
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
                .push(format!("Backup delle iscrizioni: {}", path.display())),
            Err(e) => self.log.push(format!("Backup non riuscito: {e}")),
        }
    }

    /// Riceve gli esiti di Steam e i titoli scaricati.
    fn pump(&mut self, ctx: &egui::Context) {
        if let Some(c) = &self.client {
            c.run_callbacks();
        }

        let mut finished = false;
        while let Ok(d) = self.steam_rx.try_recv() {
            let title = self.title_of(&d.id.to_string());
            let line = match d.result {
                Ok(()) => format!("✓ {}: {}", d.what, title),
                Err(e) => format!("✗ {} fallita ({}): {}", d.what, title, e),
            };
            self.log.push(line);
            self.pending = self.pending.saturating_sub(1);
            finished = true;
        }
        if finished && self.pending == 0 {
            self.refresh_local();
            self.start_meta();
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
                Ok(m) => {
                    for (k, v) in m {
                        self.meta.insert(k, v);
                    }
                }
                Err(e) => self.log.push(format!("Titoli non scaricati: {e}")),
            }
        }

        if self.pending > 0 || self.meta_rx.is_some() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    // ----- file condiviso -----

    fn publish(&mut self, ids: &[String], present: bool) -> bool {
        let Some(b) = self.backend() else {
            self.log
                .push("Scegli prima il file del gruppo (in alto).".to_string());
            return false;
        };
        let by = if self.cfg.nickname.trim().is_empty() {
            default_nick()
        } else {
            self.cfg.nickname.trim().to_string()
        };
        let items: Vec<(String, String)> =
            ids.iter().map(|i| (i.clone(), self.title_of(i))).collect();
        let res = mutate(&b, |s| {
            for (id, title) in items {
                s.items.insert(
                    id,
                    Entry {
                        present,
                        title,
                        by: by.clone(),
                        ts: now(),
                    },
                );
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

    fn add_ids(&mut self) {
        let ids = parse_ids(&self.add_input);
        if ids.is_empty() {
            self.log
                .push("Non trovo nessun ID o link valido nel campo.".to_string());
            return;
        }
        let found = match fetch_meta(&ids) {
            Ok(m) => m,
            Err(e) => {
                self.log
                    .push(format!("Controllo degli ID non riuscito: {e}"));
                return;
            }
        };
        let mut good: Vec<String> = Vec::new();
        for id in &ids {
            match found.get(id) {
                Some(m) if !m.deleted() && !m.wrong_app() => good.push(id.clone()),
                Some(m) => self.log.push(format!("{id} scartato: {}", m.warning())),
                None => self.log.push(format!("{id} scartato: ID sconosciuto")),
            }
        }
        for (k, v) in found {
            self.meta.insert(k, v);
        }
        if !good.is_empty() && self.publish(&good, true) {
            let local = self.local_set();
            for id in &good {
                if !local.contains(id) {
                    self.subscribe(id);
                }
            }
            self.log
                .push(format!("Condivisi con il gruppo: {}", good.len()));
            self.add_input.clear();
        }
    }

    // ----- azioni dell'interfaccia -----

    fn run(&mut self, a: Action) {
        match a {
            Action::Refresh => {
                self.reload_all();
                self.log.push("Lista aggiornata.".to_string());
            }
            Action::Sync => {
                if self.backend().is_none() {
                    self.log
                        .push("Scegli prima il file del gruppo (in alto).".to_string());
                    return;
                }
                self.reload_all();
                self.plan = Some(self.make_plan());
            }
            Action::ApplyPlan => {
                if let Some(plan) = self.plan.take() {
                    self.backup();
                    for (id, _, on) in plan.subscribe {
                        if on {
                            self.subscribe(&id);
                        }
                    }
                    for (id, _, on) in plan.unsubscribe {
                        if on {
                            self.unsubscribe(&id);
                        }
                    }
                }
            }
            Action::ClosePlan => self.plan = None,
            Action::AddSelected => {
                let ids: Vec<String> = self.selected.iter().cloned().collect();
                if ids.is_empty() {
                    return;
                }
                if self.publish(&ids, true) {
                    for id in &ids {
                        self.cfg.ignored.remove(id);
                    }
                    self.cfg.save();
                    let local = self.local_set();
                    for id in &ids {
                        if !local.contains(id) {
                            self.subscribe(id);
                        }
                    }
                    self.log
                        .push(format!("Condivisi con il gruppo: {}", ids.len()));
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
                    }
                }
            }
            Action::ToggleIgnore => {
                for id in self.selected.clone() {
                    if !self.cfg.ignored.remove(&id) {
                        self.cfg.ignored.insert(id);
                    }
                }
                self.cfg.save();
            }
            Action::AddIds => self.add_ids(),
            Action::SaveSettings => {
                self.cfg.shared_path = self.path_input.trim().to_string();
                self.cfg.nickname = self.nick_input.trim().to_string();
                self.cfg.save();
                if let Some(b) = self.backend() {
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
                self.reload_all();
            }
            Action::BrowseFile => {
                let mut dlg = rfd::FileDialog::new()
                    .set_title("Scegli il file del gruppo")
                    .add_filter("File JSON", &["json"]);
                if let Some(h) = dirs::home_dir() {
                    dlg = dlg.set_directory(h);
                }
                if let Some(p) = dlg.pick_file() {
                    self.path_input = p.display().to_string();
                    self.run(Action::SaveSettings);
                }
            }
            Action::NewFile => {
                let mut dlg = rfd::FileDialog::new()
                    .set_title("Crea il file del gruppo")
                    .set_file_name("gmod-workshop-sync.json")
                    .add_filter("File JSON", &["json"]);
                if let Some(h) = dirs::home_dir() {
                    dlg = dlg.set_directory(h);
                }
                if let Some(p) = dlg.save_file() {
                    self.path_input = p.display().to_string();
                    self.run(Action::SaveSettings);
                }
            }
        }
    }

    // ----- disegno -----

    fn top_panel(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>, rows: &[Row]) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            if self.client.is_some() {
                ui.label(RichText::new("✔ Collegato a Steam").color(GREEN));
            } else {
                ui.label(RichText::new(self.steam_error.clone()).color(RED));
            }
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("File del gruppo:").strong());
                ui.add(
                    egui::TextEdit::singleline(&mut self.path_input)
                        .desired_width(380.0)
                        .hint_text("nessun file scelto"),
                );
                if ui
                    .button("Sfoglia…")
                    .on_hover_text("Scegli il file del gruppo, se esiste già (te lo passa chi l'ha creato)")
                    .clicked()
                {
                    actions.push(Action::BrowseFile);
                }
                if ui
                    .button("Crea nuovo…")
                    .on_hover_text("Crea un file del gruppo nuovo e vuoto. Lo fa solo il primo del gruppo")
                    .clicked()
                {
                    actions.push(Action::NewFile);
                }
                if ui
                    .button("Salva")
                    .on_hover_text("Usa il percorso scritto qui a sinistra. Se il file non esiste lo crea")
                    .clicked()
                {
                    actions.push(Action::SaveSettings);
                }
            });
            ui.label(
                RichText::new("È il file JSON che tutti voi usate per restare allineati: mettilo in una cartella che già condividete (Dropbox, Syncthing, Drive...). Il primo del gruppo lo crea, gli altri lo scelgono con Sfoglia.")
                    .small(),
            );
            if !self.shared_error.is_empty() {
                ui.label(RichText::new(self.shared_error.clone()).color(ORANGE));
            }
            egui::CollapsingHeader::new("Altre impostazioni").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Il tuo nome (compare come autore delle modifiche):");
                    ui.add(egui::TextEdit::singleline(&mut self.nick_input).desired_width(140.0));
                    if ui.button("Salva nome").clicked() {
                        actions.push(Action::SaveSettings);
                    }
                });
            });
            let todo = rows
                .iter()
                .filter(|r| Filter::Todo.matches(r))
                .count();
            let shared_count = self.shared.items.values().filter(|e| e.present).count();
            ui.label(format!(
                "Tue iscrizioni: {}    Addon del gruppo: {}    Da sistemare: {}",
                self.local.len(),
                shared_count,
                todo
            ));
            ui.add_space(4.0);
        });
    }

    fn bottom_panel(&self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .min_height(60.0)
            .default_height(80.0)
            .show(ctx, |ui| {
                ui.label(RichText::new("Registro delle operazioni").small().strong());
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for l in &self.log {
                            ui.label(RichText::new(l.as_str()).monospace().small());
                        }
                    });
            });
    }

    fn detail_panel(&mut self, ctx: &egui::Context) {
        let Some(id) = self.current.clone() else {
            return;
        };
        let meta = self.meta.get(&id).cloned();
        let title = self.title_of(&id);
        egui::SidePanel::right("detail")
            .resizable(true)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Anteprima");
                    if ui.small_button("✕ Chiudi").clicked() {
                        self.current = None;
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.label(RichText::new(title.as_str()).strong());
                    ui.add_space(4.0);
                    match &meta {
                        Some(m) => {
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
                            if m.description.is_empty() {
                                ui.label("Nessuna descrizione.");
                            } else {
                                ui.label(m.description.as_str());
                            }
                        }
                        None => {
                            ui.label("Sto scaricando le informazioni...");
                        }
                    }
                    ui.add_space(6.0);
                    ui.hyperlink_to(
                        "Apri la pagina nel Workshop",
                        format!("https://steamcommunity.com/sharedfiles/filedetails/?id={id}"),
                    );
                });
            });
    }

    fn central_panel(&mut self, ctx: &egui::Context, rows: &[Row], actions: &mut Vec<Action>) {
        egui::CentralPanel::default().show(ctx, |ui| {
            // 1. Sincronizzazione
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(
                        RichText::new("⟳ Sincronizza con il gruppo").strong(),
                    ))
                    .on_hover_text("Confronta le tue iscrizioni con quelle del gruppo. Ti mostro cosa verrebbe aggiunto o tolto e applichi solo se confermi")
                    .clicked()
                {
                    actions.push(Action::Sync);
                }
                if ui
                    .button("Aggiorna la lista")
                    .on_hover_text("Rilegge le tue iscrizioni da Steam e il file del gruppo")
                    .clicked()
                {
                    actions.push(Action::Refresh);
                }
            });
            ui.add_space(4.0);

            // 2. Filtri
            ui.horizontal(|ui| {
                ui.label("Mostra:");
                egui::ComboBox::from_id_salt("filtro")
                    .width(280.0)
                    .selected_text(self.filter_kind.label())
                    .show_ui(ui, |ui| {
                        for f in Filter::ALL {
                            ui.selectable_value(&mut self.filter_kind, f, f.label());
                        }
                    });
                ui.label("Cerca:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter)
                        .hint_text("titolo o ID")
                        .desired_width(180.0),
                );
            });

            // 3. Azioni sugli addon selezionati
            ui.horizontal(|ui| {
                let has = !self.selected.is_empty();
                let all_ignored = has && self.selected.iter().all(|id| self.cfg.ignored.contains(id));
                ui.label(format!("Con i {} addon selezionati:", self.selected.len()));
                if ui
                    .add_enabled(has, egui::Button::new("Condividi con il gruppo"))
                    .on_hover_text("Li dai a tutto il gruppo: a ognuno compariranno come 'Manca a te' alla prossima sincronizzazione")
                    .on_disabled_hover_text("Prima spunta uno o più addon nella lista")
                    .clicked()
                {
                    actions.push(Action::AddSelected);
                }
                if ui
                    .add_enabled(has, egui::Button::new("Togli a tutto il gruppo"))
                    .on_hover_text("Li togli a tutto il gruppo: a ognuno compariranno come 'Tolto dal gruppo' alla prossima sincronizzazione. Chiede conferma")
                    .on_disabled_hover_text("Prima spunta uno o più addon nella lista")
                    .clicked()
                {
                    actions.push(Action::AskRemove);
                }
                let ignore_label = if all_ignored {
                    "Rimetti nella sincronizzazione"
                } else {
                    "Tieni solo per me"
                };
                if ui
                    .add_enabled(has, egui::Button::new(ignore_label))
                    .on_hover_text("Gli addon 'solo per te' non vengono né aggiunti né tolti dalla sincronizzazione")
                    .on_disabled_hover_text("Prima spunta uno o più addon nella lista")
                    .clicked()
                {
                    actions.push(Action::ToggleIgnore);
                }
            });

            // 4. Addon nuovo da ID o link
            ui.horizontal(|ui| {
                ui.label("Addon nuovo (link o ID dal Workshop):");
                ui.add(egui::TextEdit::singleline(&mut self.add_input).desired_width(260.0));
                if ui
                    .button("Aggiungi per tutti")
                    .on_hover_text("Controlla che l'addon esista e sia di Garry's Mod, poi lo dà a tutto il gruppo e te lo iscrive subito")
                    .clicked()
                {
                    actions.push(Action::AddIds);
                }
            });
            ui.separator();

            let f = self.filter.to_lowercase();
            let kind = self.filter_kind;
            let visible: Vec<&Row> = rows
                .iter()
                .filter(|r| {
                    kind.matches(r)
                        && (f.is_empty()
                            || r.title.to_lowercase().contains(&f)
                            || r.id.contains(&f))
                })
                .collect();

            ui.horizontal(|ui| {
                if ui.small_button("Spunta tutti quelli mostrati").clicked() {
                    for r in &visible {
                        self.selected.insert(r.id.clone());
                    }
                }
                if ui.small_button("Togli tutte le spunte").clicked() {
                    self.selected.clear();
                }
                ui.label(
                    RichText::new(format!(
                        "{} addon mostrati. Tocca un titolo per vederne l'anteprima.",
                        visible.len()
                    ))
                    .small(),
                );
            });

            if visible.is_empty() {
                ui.add_space(8.0);
                if rows.is_empty() {
                    ui.label("Nessun addon da mostrare. Se è la prima volta, scegli o crea il file del gruppo in alto e premi Salva.");
                } else {
                    ui.label("Nessun addon corrisponde al filtro scelto.");
                }
            }

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Grid::new("items")
                        .striped(true)
                        .num_columns(5)
                        .spacing([12.0, 4.0])
                        .show(ui, |ui| {
                            ui.label("");
                            ui.strong("Stato");
                            ui.strong("Titolo");
                            ui.strong("ID");
                            ui.strong("Avviso");
                            ui.end_row();
                            for r in &visible {
                                let mut sel = self.selected.contains(&r.id);
                                if ui.checkbox(&mut sel, "").changed() {
                                    if sel {
                                        self.selected.insert(r.id.clone());
                                    } else {
                                        self.selected.remove(&r.id);
                                    }
                                }
                                let (txt, col) = status_style(r.status);
                                ui.label(RichText::new(txt).color(col));
                                let is_current = self.current.as_deref() == Some(r.id.as_str());
                                if ui
                                    .selectable_label(is_current, r.title.as_str())
                                    .on_hover_text("Tocca per vedere immagine e descrizione")
                                    .clicked()
                                {
                                    self.current = Some(r.id.clone());
                                }
                                ui.label(RichText::new(r.id.as_str()).monospace());
                                ui.label(RichText::new(r.warn.as_str()).color(ORANGE));
                                ui.end_row();
                            }
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
            .show(ctx, |ui| {
                if plan.subscribe.is_empty() && plan.unsubscribe.is_empty() {
                    ui.label("Sei già allineato con il gruppo: niente da fare.");
                }
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        if !plan.subscribe.is_empty() {
                            ui.strong(format!(
                                "Verranno aggiunti alle tue iscrizioni ({})",
                                plan.subscribe.len()
                            ));
                            for (_, title, on) in plan.subscribe.iter_mut() {
                                ui.checkbox(on, title.as_str());
                            }
                        }
                        if !plan.unsubscribe.is_empty() {
                            ui.add_space(6.0);
                            ui.strong(format!(
                                "Verranno tolti dalle tue iscrizioni ({})",
                                plan.unsubscribe.len()
                            ));
                            for (_, title, on) in plan.unsubscribe.iter_mut() {
                                ui.checkbox(on, title.as_str());
                            }
                        }
                    });
                ui.separator();
                let n = plan.subscribe.iter().filter(|x| x.2).count()
                    + plan.unsubscribe.iter().filter(|x| x.2).count();
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
                    RichText::new("Meglio a Garry's Mod chiuso. Prima di applicare salvo un backup delle tue iscrizioni.")
                        .small(),
                );
            });
    }

    fn confirm_window(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(ids) = self.confirm_remove.clone() else {
            return;
        };
        let titles: Vec<String> = ids.iter().take(8).map(|i| self.title_of(i)).collect();
        egui::Window::new("Conferma")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!("Togliere {} addon a TUTTO il gruppo?", ids.len()));
                ui.label("Chi sincronizza dopo di te li perderà dalle sue iscrizioni.");
                for t in &titles {
                    ui.label(format!("• {t}"));
                }
                if ids.len() > titles.len() {
                    ui.label(format!("… e altri {}", ids.len() - titles.len()));
                }
                ui.horizontal(|ui| {
                    if ui.button("Sì, togli a tutti").clicked() {
                        actions.push(Action::ConfirmRemove);
                    }
                    if ui.button("Annulla").clicked() {
                        actions.push(Action::CancelRemove);
                    }
                });
            });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump(ctx);
        let rows = self.build_rows();
        let mut actions: Vec<Action> = Vec::new();
        self.top_panel(ctx, &mut actions, &rows);
        self.bottom_panel(ctx);
        self.detail_panel(ctx);
        self.central_panel(ctx, &rows, &mut actions);
        self.plan_window(ctx, &mut actions);
        self.confirm_window(ctx, &mut actions);
        for a in actions {
            self.run(a);
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 680.0])
            .with_min_inner_size([760.0, 440.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Garry's Mod Workshop Sync",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
