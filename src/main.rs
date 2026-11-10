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

// ---------------------------------------------------------------------
// Informazioni sugli oggetti (API web pubblica di Steam, senza chiave)
// ---------------------------------------------------------------------

#[derive(Clone, Default)]
struct Meta {
    title: String,
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
// Righe, piano di sincronizzazione, azioni
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
        Status::Missing => ("manca a te", Color32::from_rgb(230, 150, 40)),
        Status::Remove => ("da rimuovere", Color32::from_rgb(210, 70, 70)),
        Status::LocalOnly => ("solo tuo", Color32::from_rgb(70, 140, 220)),
        Status::Ok => ("ok", Color32::from_rgb(60, 160, 80)),
        Status::Ignored => ("ignorato", Color32::GRAY),
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
    filter: String,
    only_todo: bool,
    add_input: String,
    plan: Option<Plan>,
    confirm_remove: Option<Vec<String>>,
    log: Vec<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
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
            filter: String::new(),
            only_todo: false,
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
                self.shared_error = "Imposta il percorso del file condiviso.".to_string();
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
                .push("Imposta prima il file condiviso.".to_string());
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
                    .push(format!("Salvataggio nel file condiviso fallito: {e}"));
                false
            }
        }
    }

    fn add_ids(&mut self) {
        let ids = parse_ids(&self.add_input);
        if ids.is_empty() {
            self.log.push("Nessun ID valido nel campo.".to_string());
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
            self.log.push(format!("Aggiunti a tutti: {}", good.len()));
            self.add_input.clear();
        }
    }

    // ----- azioni dell'interfaccia -----

    fn run(&mut self, a: Action) {
        match a {
            Action::Refresh => {
                self.reload_all();
                self.log.push("Aggiornato.".to_string());
            }
            Action::Sync => {
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
                    self.log.push(format!("Aggiunti a tutti: {}", ids.len()));
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
                        self.log.push(format!("Rimossi per tutti: {}", ids.len()));
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
                self.reload_all();
            }
        }
    }

    // ----- disegno -----

    fn top_panel(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>, rows: &[Row]) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            if self.client.is_some() {
                ui.label(
                    RichText::new("Steam: connesso (Garry's Mod)")
                        .color(Color32::from_rgb(60, 160, 80)),
                );
            } else {
                ui.label(
                    RichText::new(self.steam_error.clone()).color(Color32::from_rgb(210, 70, 70)),
                );
            }
            ui.horizontal(|ui| {
                ui.label("File condiviso:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.path_input)
                        .desired_width(360.0)
                        .hint_text("es. D:\\Dropbox\\gmod-workshop.json"),
                );
                ui.label("Nome:");
                ui.add(egui::TextEdit::singleline(&mut self.nick_input).desired_width(110.0));
                if ui.button("Salva").clicked() {
                    actions.push(Action::SaveSettings);
                }
            });
            if !self.shared_error.is_empty() {
                ui.label(
                    RichText::new(self.shared_error.clone()).color(Color32::from_rgb(230, 150, 40)),
                );
            }
            let todo = rows
                .iter()
                .filter(|r| {
                    matches!(
                        r.status,
                        Status::Missing | Status::Remove | Status::LocalOnly
                    )
                })
                .count();
            let shared_count = self.shared.items.values().filter(|e| e.present).count();
            ui.label(format!(
                "Iscritti: {}    Nel gruppo: {}    Da sistemare: {}",
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
            .min_height(70.0)
            .default_height(100.0)
            .show(ctx, |ui| {
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

    fn central_panel(&mut self, ctx: &egui::Context, rows: &[Row], actions: &mut Vec<Action>) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter)
                        .hint_text("Cerca per titolo o ID...")
                        .desired_width(220.0),
                );
                ui.checkbox(&mut self.only_todo, "Solo da sistemare");
                if ui.button("Aggiorna").clicked() {
                    actions.push(Action::Refresh);
                }
                if ui.button("Sincronizza…").clicked() {
                    actions.push(Action::Sync);
                }
            });
            ui.horizontal(|ui| {
                let has = !self.selected.is_empty();
                if ui
                    .add_enabled(has, egui::Button::new("Aggiungi a tutti"))
                    .clicked()
                {
                    actions.push(Action::AddSelected);
                }
                if ui
                    .add_enabled(has, egui::Button::new("Rimuovi a tutti"))
                    .clicked()
                {
                    actions.push(Action::AskRemove);
                }
                if ui
                    .add_enabled(has, egui::Button::new("Ignora / non ignorare"))
                    .clicked()
                {
                    actions.push(Action::ToggleIgnore);
                }
                ui.separator();
                ui.label("ID o link:");
                ui.add(egui::TextEdit::singleline(&mut self.add_input).desired_width(220.0));
                if ui.button("Aggiungi ID a tutti").clicked() {
                    actions.push(Action::AddIds);
                }
            });
            ui.separator();

            let f = self.filter.to_lowercase();
            let only_todo = self.only_todo;
            let visible: Vec<&Row> = rows
                .iter()
                .filter(|r| {
                    (!only_todo || !matches!(r.status, Status::Ok | Status::Ignored))
                        && (f.is_empty()
                            || r.title.to_lowercase().contains(&f)
                            || r.id.contains(&f))
                })
                .collect();

            ui.horizontal(|ui| {
                if ui.small_button("Seleziona visibili").clicked() {
                    for r in &visible {
                        self.selected.insert(r.id.clone());
                    }
                }
                if ui.small_button("Deseleziona").clicked() {
                    self.selected.clear();
                }
                ui.label(format!(
                    "{} elementi, {} selezionati",
                    visible.len(),
                    self.selected.len()
                ));
            });

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
                                ui.label(r.title.as_str());
                                ui.label(RichText::new(r.id.as_str()).monospace());
                                ui.label(
                                    RichText::new(r.warn.as_str())
                                        .color(Color32::from_rgb(210, 140, 40)),
                                );
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
        egui::Window::new("Anteprima sincronizzazione")
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
                            ui.strong(format!("Da iscrivere ({})", plan.subscribe.len()));
                            for (_, title, on) in plan.subscribe.iter_mut() {
                                ui.checkbox(on, title.as_str());
                            }
                        }
                        if !plan.unsubscribe.is_empty() {
                            ui.add_space(6.0);
                            ui.strong(format!("Da disiscrivere ({})", plan.unsubscribe.len()));
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
                    RichText::new("Meglio a Garry's Mod chiuso. Prima di applicare salvo un backup delle iscrizioni.")
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
                ui.label(format!(
                    "Rimuovere {} elementi per TUTTO il gruppo?",
                    ids.len()
                ));
                ui.label("Chi sincronizza dopo di te li perderà dalle iscrizioni.");
                for t in &titles {
                    ui.label(format!("• {t}"));
                }
                if ids.len() > titles.len() {
                    ui.label(format!("… e altri {}", ids.len() - titles.len()));
                }
                ui.horizontal(|ui| {
                    if ui.button("Rimuovi").clicked() {
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
            .with_inner_size([980.0, 640.0])
            .with_min_inner_size([720.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Garry's Mod Workshop Sync",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
