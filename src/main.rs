//! # YOL — «Ё лайт»
//!
//! GUI-часть: главное окно на `egui`/`eframe`, фоновый рабочий поток и
//! консольный потоковый режим. Вся логика ёфикации живёт в [`yol::yo_core`].
//!
//! ## Архитектура
//!
//! ```text
//!   ┌──────────── главный поток (egui) ────────────┐       ┌── рабочий поток ──┐
//!   │  YoApp::update()                             │  mpsc │                   │
//!   │    ├ pump(): забирает Event из канала     ◄──┼───────┤  Engine::process  │
//!   │    ├ рисует панель запроса                   │       │   через GuiSink   │
//!   │    └ ans_tx.send(AskDecision) ───────────────┼──────►│  (стоит на ответе)│
//!   └──────────────────────────────────────────────┘       └───────────────────┘
//! ```
//!
//! Главный поток **никогда** не ждёт рабочий: готовый текст приходит порциями
//! по 64 КБ, статистика — примерно 15 раз в секунду. Поэтому интерфейс не
//! «фризится» даже на стомегабайтных файлах.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::Instant;

// Ядро живёт в библиотеке `yol` (src/lib.rs → src/yo_core.rs), а не подключается
// модулем: так его же можно покрывать интеграционными тестами из tests/.
use yol::yo_core::{
    clear_user_selections, database_info, decode_bytes, decode_cp1251, encode_string, preload,
    AmbiguousPolicy, AskDecision, AskRequest, Engine, Sink, Stats, TextEncoding, YoOptions,
};

// ═══════════════════════════════════════════════════════════════════════════
//  Константы интерфейса
// ═══════════════════════════════════════════════════════════════════════════

const APP_NAME: &str = "YOL — Ё лайт";

/// Версия для окна «О программе» и `yol --version`.
const VERSION: &str = yol::yo_core::YO_VERSION;

/// Выше этой длины редактор egui начинает ощутимо тормозить — предлагаем
/// файловый режим (он потоковый и ничем не ограничен).
const MAX_EDITOR_CHARS: usize = 4_000_000;

/// Выше этой длины отключаем подсветку: `LayoutJob` строится за O(меток).
const MAX_HIGHLIGHT_CHARS: usize = 400_000;

/// Ограничение на число запоминаемых пометок в GUI.
const MAX_GUI_MARKS: usize = 200_000;

/// Сколько строк хранить в журнале замен.
const MAX_JOURNAL: usize = 400;

const COLOR_MARK: egui::Color32 = egui::Color32::from_rgb(255, 208, 96);
const COLOR_MARK_BG: egui::Color32 = egui::Color32::from_rgb(96, 62, 12);
const COLOR_YE: egui::Color32 = egui::Color32::from_rgb(120, 190, 255);
const COLOR_YO: egui::Color32 = egui::Color32::from_rgb(150, 240, 150);
const COLOR_DIM: egui::Color32 = egui::Color32::from_rgb(150, 150, 160);

// ═══════════════════════════════════════════════════════════════════════════
//  Канал событий рабочего потока
// ═══════════════════════════════════════════════════════════════════════════

/// Всё, что рабочий поток сообщает интерфейсу.
#[allow(dead_code)]
enum Event {
    /// Словари разобраны — можно показать сводку о базах.
    Ready {
        words: usize,
        dic_version: String,
        rules: usize,
        re_version: String,
        elapsed_ms: u128,
    },
    /// Очередная порция результата.
    Chunk {
        base: usize,
        text: String,
        marks: Vec<(usize, usize)>,
    },
    /// Промежуточная статистика.
    Stats(Stats),
    /// Порция журнала замен.
    Journal(Vec<(String, String)>),
    /// Вопрос к пользователю. Рабочий поток заблокирован до ответа.
    Ask(AskRequest),
    /// Откат уже отправленной замены.
    Undo {
        offset: usize,
        old: String,
        new: String,
    },
    /// Работа завершена.
    Done { stats: Stats },
    /// Что-то пошло не так.
    Failed(String),
}

/// Живой прогон ёфикации.
struct Run {
    rx: Receiver<Event>,
    ans_tx: Sender<AskDecision>,
    cancel: Arc<AtomicBool>,
    total_chars: usize,
}

/// Реализация [`Sink`], которая пересылает всё в GUI и ждёт его ответов.
struct GuiSink<'a> {
    tx: &'a Sender<Event>,
    rx: &'a Receiver<AskDecision>,
    cancel: &'a AtomicBool,
}

impl Sink for GuiSink<'_> {
    fn chunk(&mut self, base: usize, text: &str, marks: &[(usize, usize)]) {
        let _ = self.tx.send(Event::Chunk {
            base,
            text: text.to_owned(),
            marks: marks.to_vec(),
        });
    }

    fn stats(&mut self, stats: Stats) {
        let _ = self.tx.send(Event::Stats(stats));
    }

    fn journal(&mut self, entries: &[(String, String)]) {
        if !entries.is_empty() {
            let _ = self.tx.send(Event::Journal(entries.to_vec()));
        }
    }

    /// Рабочий поток засыпает здесь ровно до того момента, пока пользователь
    /// не нажмёт одну из кнопок панели запроса.
    fn ask(&mut self, req: AskRequest) -> AskDecision {
        if self.tx.send(Event::Ask(req)).is_err() {
            return AskDecision::Cancel;
        }
        self.rx.recv().unwrap_or(AskDecision::Cancel)
    }

    fn undo(&mut self, offset: usize, old: &str, new: &str) {
        let _ = self.tx.send(Event::Undo {
            offset,
            old: old.to_owned(),
            new: new.to_owned(),
        });
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Встроенный диалог работы с файлами (без GTK/WinAPI — чистый std::fs)
// ═══════════════════════════════════════════════════════════════════════════

enum FileAction {
    Load(PathBuf, TextEncoding),
    Save(PathBuf, TextEncoding),
}

struct FileDialog {
    open: bool,
    is_save: bool,
    dir: PathBuf,
    name: String,
    entries: Vec<(String, bool)>,
    encoding: TextEncoding,
    status: String,
}

impl FileDialog {
    fn new() -> Self {
        let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut d = Self {
            open: false,
            is_save: false,
            dir,
            name: String::new(),
            entries: Vec::new(),
            encoding: TextEncoding::Utf8Bom,
            status: String::new(),
        };
        d.refresh();
        d
    }

    fn refresh(&mut self) {
        self.entries = list_dir(&self.dir);
    }

    fn path(&self) -> PathBuf {
        self.dir.join(&self.name)
    }

    fn open_for_load(&mut self) {
        self.open = true;
        self.is_save = false;
        self.status.clear();
        self.refresh();
    }

    fn open_for_save(&mut self, suggested: &str) {
        self.open = true;
        self.is_save = true;
        self.status.clear();
        self.name = suggested.to_owned();
        self.refresh();
    }
}

/// Содержимое каталога: сначала папки, потом файлы, всё по алфавиту.
fn list_dir(dir: &Path) -> Vec<(String, bool)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            dirs.push(name);
        } else {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();
    dirs.into_iter()
        .map(|n| (n, true))
        .chain(files.into_iter().map(|n| (n, false)))
        .collect()
}

/// Декодировать с явным указанием формата (без BOM-эвристики).
fn decode_with(bytes: &[u8], enc: TextEncoding) -> String {
    let has_bom = |prefix: &[u8]| bytes.len() >= prefix.len() && &bytes[..prefix.len()] == prefix;
    match enc {
        TextEncoding::Cp1251 => decode_cp1251(bytes),
        TextEncoding::Utf8 | TextEncoding::Utf8Bom => {
            let skip = if has_bom(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
            String::from_utf8_lossy(&bytes[skip..]).into_owned()
        }
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            let (skip, le) = if has_bom(&[0xFF, 0xFE]) {
                (2, true)
            } else if has_bom(&[0xFE, 0xFF]) {
                (2, false)
            } else {
                (0, enc == TextEncoding::Utf16Le)
            };
            let units: Vec<u16> = bytes[skip..]
                .chunks_exact(2)
                .map(|c| {
                    if le {
                        u16::from_le_bytes([c[0], c[1]])
                    } else {
                        u16::from_be_bytes([c[0], c[1]])
                    }
                })
                .collect();
            String::from_utf16_lossy(&units)
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Приложение
// ═══════════════════════════════════════════════════════════════════════════

struct YoApp {
    // ── данные ──────────────────────────────────────────────────────────
    opts: YoOptions,
    input: String,
    output: String,
    marks: Vec<(usize, usize)>,
    /// Журнал замен («было → стало»).
    journal: Vec<(String, String)>,

    // ── состояние ───────────────────────────────────────────────────────
    run: Option<Run>,
    ask: Option<AskRequest>,
    stats: Stats,
    dict_info: Option<(usize, String, usize, String, u128)>,
    status_left: String,

    // ── интерфейс ───────────────────────────────────────────────────────
    dark: bool,
    /// Какая тема уже применена к стилю (`None` — ещё ни одна).
    applied_dark: Option<bool>,
    split: f32,
    highlight: bool,
    show_journal: bool,
    show_options: bool,
    show_about: bool,
    show_stats: bool,
    dialog: FileDialog,

    /// Какой файл сейчас загружен (для «Сохранить» без диалога).
    current_file: Option<PathBuf>,
    current_encoding: TextEncoding,
}

impl YoApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Тёмная тема — сразу и по умолчанию.
        setup_fonts(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx, true);

        // Словари (1.8 МБ) разбираем в фоне: окно появляется мгновенно,
        // а первый прогон всё равно дождётся готовности базы.
        std::thread::spawn(preload);

        Self {
            opts: YoOptions::default(),
            input: String::new(),
            output: String::new(),
            marks: Vec::new(),
            journal: Vec::new(),
            run: None,
            ask: None,
            stats: Stats::default(),
            dict_info: None,
            status_left: "готов — вставьте текст или загрузите файл".to_owned(),
            dark: true,
            applied_dark: None,
            split: 0.5,
            highlight: true,
            show_journal: false,
            show_options: false,
            show_about: false,
            show_stats: false,
            dialog: FileDialog::new(),
            current_file: None,
            current_encoding: TextEncoding::Utf8Bom,
        }
    }

    #[inline]
    fn busy(&self) -> bool {
        self.run.is_some()
    }

    // ────────────────────────────────────────────────────────────────────
    //  Запуск / остановка / обмен с рабочим потоком
    // ────────────────────────────────────────────────────────────────────

    fn start(&mut self) {
        if self.busy() {
            return;
        }
        if self.input.is_empty() {
            self.status_left = "нечего ёфицировать".to_owned();
            return;
        }

        let (tx, rx) = mpsc::channel::<Event>();
        let (ans_tx, ans_rx) = mpsc::channel::<AskDecision>();
        let cancel = Arc::new(AtomicBool::new(false));

        let input = self.input.clone();
        let opts = self.opts.clone();
        let want_journal = self.show_journal;
        let cancel_thread = cancel.clone();
        let total_chars = self.input.chars().count();

        std::thread::spawn(move || {
            let t0 = Instant::now();
            let (words, dic_version, rules, re_version) = database_info();
            let _ = tx.send(Event::Ready {
                words,
                dic_version,
                rules,
                re_version,
                elapsed_ms: t0.elapsed().as_millis(),
            });

            let mut engine = Engine::new(opts);
            engine.set_journal(want_journal);
            let mut sink = GuiSink {
                tx: &tx,
                rx: &ans_rx,
                cancel: &cancel_thread,
            };
            let stats = engine.process(&input, &mut sink);
            let _ = tx.send(Event::Done { stats });
        });

        self.output.clear();
        self.marks.clear();
        self.journal.clear();
        self.stats = Stats::default();
        self.ask = None;
        self.run = Some(Run {
            rx,
            ans_tx,
            cancel,
            total_chars,
        });
        self.status_left = "ёфикация…".to_owned();
    }

    fn stop(&mut self) {
        let tx = match &self.run {
            Some(r) => {
                r.cancel.store(true, Ordering::Relaxed);
                Some(r.ans_tx.clone())
            }
            None => None,
        };
        // Если рабочий поток стоит на вопросе — снимаем его с паузы.
        if let Some(tx) = tx {
            let _ = tx.send(AskDecision::Cancel);
        }
        self.ask = None;
        self.status_left = "прервано".to_owned();
    }

    /// Забрать всё, что накопилось в канале (не блокируя главный поток).
    fn pump(&mut self) {
        let Some(run) = self.run.take() else {
            return;
        };
        let mut finished = false;
        loop {
            match run.rx.try_recv() {
                Ok(Event::Ready {
                    words,
                    dic_version,
                    rules,
                    re_version,
                    elapsed_ms,
                }) => {
                    self.dict_info = Some((words, dic_version, rules, re_version, elapsed_ms));
                }
                Ok(Event::Chunk { base, text, marks }) => {
                    if self.marks.len() < MAX_GUI_MARKS {
                        for (s, e) in &marks {
                            self.marks.push((base + s, base + e));
                        }
                    }
                    self.output.push_str(&text);
                }
                Ok(Event::Journal(entries)) => {
                    for pair in entries {
                        if self.journal.len() >= MAX_JOURNAL {
                            self.journal.remove(0);
                        }
                        self.journal.push(pair);
                    }
                }
                Ok(Event::Stats(s)) => self.stats = s,
                Ok(Event::Ask(req)) => self.ask = Some(req),
                Ok(Event::Undo { offset, old, new }) => self.patch_output(offset, &old, &new),
                Ok(Event::Done { stats }) => {
                    self.stats = stats;
                    finished = true;
                }
                Ok(Event::Failed(msg)) => {
                    self.status_left = msg;
                    finished = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            self.ask = None;
            let changed = self.stats.checked_changed;
            self.status_left = format!("готово · заменено {changed}");
        } else {
            self.run = Some(run);
        }
    }

    /// Откатить уже показанную замену прямо в строке результата.
    fn patch_output(&mut self, offset: usize, old: &str, new: &str) {
        let end = match offset.checked_add(new.len()) {
            Some(e) if e <= self.output.len() => e,
            _ => return,
        };
        if &self.output[offset..end] != new {
            return;
        }
        self.output.replace_range(offset..end, old);
        // Откат может изменить длину текста — сдвигаем все метки после него.
        let delta = old.len() as i64 - new.len() as i64;
        if delta != 0 {
            for (s, e) in self.marks.iter_mut() {
                if *s >= end {
                    *s = (*s as i64 + delta) as usize;
                    *e = (*e as i64 + delta) as usize;
                }
            }
        }
        self.marks.retain(|&(s, _)| !(s >= offset && s < end));
    }

    /// Ответить рабочему потоку на панель запроса.
    fn answer(&mut self, decision: AskDecision) {
        let tx = match &self.run {
            Some(r) => r.ans_tx.clone(),
            None => return,
        };
        if let Some(req) = self.ask.take() {
            let chosen = match decision {
                AskDecision::Ye | AskDecision::YeAll => Some(req.word1.clone()),
                AskDecision::Yo | AskDecision::YoAll => Some(req.word2.clone()),
                AskDecision::Undo | AskDecision::Cancel => None,
            };
            if let Some(chosen) = chosen {
                if self.journal.len() >= MAX_JOURNAL {
                    self.journal.remove(0);
                }
                self.journal.push((req.word1, chosen));
            }
        }
        let _ = tx.send(decision);
    }

    // ────────────────────────────────────────────────────────────────────
    //  Файлы
    // ────────────────────────────────────────────────────────────────────

    fn load_path(&mut self, path: PathBuf, enc: Option<TextEncoding>) {
        match std::fs::read(&path) {
            Ok(bytes) => {
                let (text, detected) = match enc {
                    Some(e) => (decode_with(&bytes, e), e),
                    None => decode_bytes(&bytes),
                };
                let n = text.chars().count();
                self.input = text;
                self.output.clear();
                self.marks.clear();
                self.journal.clear();
                self.current_file = Some(path.clone());
                self.current_encoding = detected;
                self.status_left = format!(
                    "загружено: {} · {} · {} симв.",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    detected.label(),
                    n
                );
            }
            Err(e) => self.status_left = format!("ошибка чтения: {e}"),
        }
    }

    fn save_path(&mut self, path: PathBuf, enc: TextEncoding) {
        let bytes = encode_string(&self.output, enc);
        match std::fs::write(&path, bytes) {
            Ok(()) => {
                self.current_file = Some(path.clone());
                self.current_encoding = enc;
                self.status_left = format!(
                    "сохранено: {} · {}",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    enc.label()
                );
            }
            Err(e) => self.status_left = format!("ошибка записи: {e}"),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Шрифты и стиль
// ═══════════════════════════════════════════════════════════════════════════

/// Гарантируем кириллицу в моноширинном шрифте: в egui `Monospace` по
/// умолчанию — Хэк, которого в минимальных сборках может не быть с кириллицей.
/// Подменяем его тем же набором, что у `Proportional` (Ubuntu-Light покрывает
/// кириллицу полностью).
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    if let Some(names) = fonts.families.get(&egui::FontFamily::Proportional).cloned() {
        fonts.families.insert(egui::FontFamily::Monospace, names);
    }
    ctx.set_fonts(fonts);
}

/// Тема + стиль. `set_visuals` заменяет `Visuals` **целиком**, поэтому стиль
/// накладывается строго после него — иначе скругления виджетов сбрасываются.
fn apply_theme(ctx: &egui::Context, dark: bool) {
    ctx.set_visuals(if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    });
    setup_style(ctx);
}

/// Современная «тёмная» подача: скругления, воздух, читаемый кегль.
fn setup_style(ctx: &egui::Context) {
    ctx.style_mut(|style| {
        let r = egui::CornerRadius::same(6);
        style.visuals.widgets.noninteractive.corner_radius = r;
        style.visuals.widgets.inactive.corner_radius = r;
        style.visuals.widgets.hovered.corner_radius = r;
        style.visuals.widgets.active.corner_radius = r;
        style.visuals.widgets.open.corner_radius = r;
        style.visuals.window_corner_radius = egui::CornerRadius::same(10);
        style.visuals.menu_corner_radius = egui::CornerRadius::same(8);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);

        // `fn_call_width` у rustfmt по умолчанию 60, поэтому аргументы
        // записываем вертикально — так файл остаётся стабильным под `cargo fmt`.
        let ts = &mut style.text_styles;
        ts.insert(
            egui::TextStyle::Small,
            egui::FontId::new(12.0, egui::FontFamily::Proportional),
        );
        ts.insert(
            egui::TextStyle::Body,
            egui::FontId::new(14.0, egui::FontFamily::Proportional),
        );
        ts.insert(
            egui::TextStyle::Button,
            egui::FontId::new(14.0, egui::FontFamily::Proportional),
        );
        ts.insert(
            egui::TextStyle::Heading,
            egui::FontId::new(19.0, egui::FontFamily::Proportional),
        );
        ts.insert(
            egui::TextStyle::Monospace,
            egui::FontId::new(14.5, egui::FontFamily::Monospace),
        );
    });
}

// ═══════════════════════════════════════════════════════════════════════════
//  Реализация eframe::App
// ═══════════════════════════════════════════════════════════════════════════

impl eframe::App for YoApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Тёмная тема — принудительно (по умолчанию). `set_visuals` каждый кадр
        // клонировал бы Arc<Style>, поэтому применяем только при смене режима.
        if self.applied_dark != Some(self.dark) {
            apply_theme(ctx, self.dark);
            self.applied_dark = Some(self.dark);
        }

        self.pump();
        if self.busy() {
            // Иначе egui не будет перерисовываться, пока не придёт ввод.
            ctx.request_repaint();
        }

        self.menu_bar(ctx);
        self.options_strip(ctx);
        // Нижние панели накладываются в порядке вызова: первая — самая нижняя.
        self.status_bar(ctx);
        if self.ask.is_some() {
            self.ask_panel(ctx);
        }
        self.central_panes(ctx);

        self.file_dialog_window(ctx);
        self.options_window(ctx);
        self.about_window(ctx);
        self.stats_window(ctx);
        if self.show_journal {
            self.journal_window(ctx);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Отрисовка
// ═══════════════════════════════════════════════════════════════════════════

impl YoApp {
    // ── главное меню ───────────────────────────────────────────────────
    fn menu_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("Файл", |ui| {
                    if ui.button("Загрузить текст…").clicked() {
                        self.dialog.open_for_load();
                        ui.close_menu();
                    }
                    if ui
                        .add_enabled(
                            !self.output.is_empty(),
                            egui::Button::new("Сохранить результат…"),
                        )
                        .clicked()
                    {
                        let suggested = self
                            .current_file
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .map(|n| format!("yo_{}", n.to_string_lossy()))
                            .unwrap_or_else(|| "yofied.txt".to_owned());
                        self.dialog.encoding = self.current_encoding;
                        self.dialog.open_for_save(&suggested);
                        ui.close_menu();
                    }
                    ui.separator();
                    if let Some(path) = &self.current_file {
                        ui.label(
                            egui::RichText::new(path.to_string_lossy().into_owned())
                                .small()
                                .color(COLOR_DIM),
                        );
                    }
                    ui.separator();
                    if ui.button("Выход").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });

                ui.menu_button("Правка", |ui| {
                    if ui.button("Очистить всё").clicked() {
                        self.input.clear();
                        self.output.clear();
                        self.marks.clear();
                        self.journal.clear();
                        ui.close_menu();
                    }
                    if ui.button("Результат → исходник").clicked() {
                        self.input = std::mem::take(&mut self.output);
                        self.output.clear();
                        self.marks.clear();
                        ui.close_menu();
                    }
                    if ui.button("Копировать результат").clicked() {
                        ctx.copy_text(self.output.clone());
                        ui.close_menu();
                    }
                });

                ui.menu_button("Ёфикация", |ui| {
                    if ui
                        .add_enabled(
                            !self.busy() && !self.input.is_empty(),
                            egui::Button::new("Ёфицировать"),
                        )
                        .clicked()
                    {
                        self.start();
                        ui.close_menu();
                    }
                    if ui
                        .add_enabled(self.busy(), egui::Button::new("Прервать"))
                        .clicked()
                    {
                        self.stop();
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Сбросить «всегда Е/Ё»").clicked() {
                        clear_user_selections();
                        self.status_left = "память ответов очищена".to_owned();
                        ui.close_menu();
                    }
                });

                ui.menu_button("Настройки", |ui| {
                    if ui.checkbox(&mut self.dark, "Тёмная тема").clicked() {
                        ui.close_menu();
                    }
                    if ui
                        .checkbox(&mut self.show_journal, "Журнал замен")
                        .clicked()
                    {
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Все опции…").clicked() {
                        self.show_options = true;
                        ui.close_menu();
                    }
                });

                ui.menu_button("Помощь", |ui| {
                    if ui.button("Статистика…").clicked() {
                        self.show_stats = true;
                        ui.close_menu();
                    }
                    if ui.button("О программе").clicked() {
                        self.show_about = true;
                        ui.close_menu();
                    }
                });

                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| match &self.dict_info {
                        Some((words, _, rules, _, _)) => {
                            ui.label(
                                egui::RichText::new(format!("слов: {words} · правил: {rules}"))
                                    .small()
                                    .color(COLOR_DIM),
                            );
                        }
                        None => {
                            ui.label(
                                egui::RichText::new("словарь загружается…")
                                    .small()
                                    .color(COLOR_DIM),
                            );
                        }
                    },
                );
            });
        });
    }

    /// Верхняя лента: все флажки ёфикации + главная кнопка «Ёфицировать».
    fn options_strip(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("options_strip").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                // ── главная кнопка ──────────────────────────────────────
                let btn =
                    egui::Button::new(egui::RichText::new("Ёфицировать").strong().color(COLOR_YO));
                if ui.add_enabled(!self.busy(), btn).clicked() {
                    self.start();
                }
                if ui
                    .add_enabled(self.busy(), egui::Button::new("Стоп"))
                    .clicked()
                {
                    self.stop();
                }
                ui.separator();

                // ── флажки вкладки «Ёфикация» ───────────────────────────
                let o = &mut self.opts;
                ui.checkbox(&mut o.reg_exprs, "Рег. выражения")
                    .on_hover_text("Использование словаря регулярных выражений");
                ui.checkbox(&mut o.no_var_only, "Только безопасные")
                    .on_hover_text("Только беспоспорные замены");
                ui.checkbox(&mut o.var_only, "Только интерактивные")
                    .on_hover_text("Только интерактивные замены");
                ui.checkbox(&mut o.check_yo, "Проверять «ё»")
                    .on_hover_text("Проверять расстановку «ё»");
                ui.checkbox(&mut o.always_ask, "Всегда подтверждать")
                    .on_hover_text("Всегда подтверждать замену");
                ui.separator();
                ui.checkbox(&mut o.to_confirm_abbr, "…для сокращений")
                    .on_hover_text("Всегда подтверждать замену для сокращений");
                ui.checkbox(&mut o.to_confirm_cap, "…для имён")
                    .on_hover_text("Всегда подтверждать замену для имён собственных");
                ui.checkbox(&mut o.to_confirm_ellipsis, "…для оборванных")
                    .on_hover_text("Всегда подтверждать замену для оборванных слов");
                ui.separator();
                ui.checkbox(&mut o.propose_last, "Последний вариант")
                    .on_hover_text("Предлагать последний вариант замены");
                ui.checkbox(&mut o.fb_format, "FictionBook")
                    .on_hover_text("Учитывать формат FictionBook (не трогать <binary>)");
                ui.checkbox(&mut o.mark, "Помечать")
                    .on_hover_text("Помечать исправленное");
                ui.checkbox(&mut o.word_wrap, "Перенос строк")
                    .on_hover_text("Переносить текст по словам");
                ui.separator();

                // ── политика для неоднозначных слов ─────────────────────
                ui.label("Неоднозначные:");
                if ui
                    .button(self.opts.ambiguous.label())
                    .on_hover_text("Слова, помеченные «?»: клик переключает")
                    .clicked()
                {
                    self.opts.ambiguous = next_policy(self.opts.ambiguous);
                }
            });
            ui.add_space(4.0);
        });
    }

    /// Две панели с текстом и перетаскиваемый разделитель.
    fn central_panes(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let avail = ui.available_size();
            let sep = 6.0f32;
            let usable = (avail.x - sep).max(240.0);
            let left_w = (usable * self.split).max(120.0);
            let right_w = (usable - left_w).max(120.0);

            ui.horizontal(|ui| {
                ui.allocate_ui(egui::vec2(left_w, avail.y), |ui| {
                    self.editor_pane(ui, true);
                });

                let (rect, response) =
                    ui.allocate_exact_size(egui::vec2(sep, avail.y), egui::Sense::drag());
                if response.dragged() {
                    self.split = (self.split + response.drag_delta().x / usable).clamp(0.15, 0.85);
                }
                let c = if response.hovered() || response.dragged() {
                    ui.visuals().widgets.hovered.bg_fill
                } else {
                    ui.visuals().widgets.noninteractive.bg_fill
                };
                ui.painter()
                    .rect_filled(rect, egui::CornerRadius::same(3), c);

                ui.allocate_ui(egui::vec2(right_w, avail.y), |ui| {
                    self.editor_pane(ui, false);
                });
            });
        });
    }

    fn editor_pane(&mut self, ui: &mut egui::Ui, is_input: bool) {
        let title = if is_input {
            "Исходный текст"
        } else {
            "Результат (ёфицированный)"
        };
        let chars = if is_input {
            self.input.chars().count()
        } else {
            self.output.chars().count()
        };

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(title).strong());
            ui.label(
                egui::RichText::new(format!("{chars} симв."))
                    .small()
                    .color(COLOR_DIM),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut copy = false;
                let mut clear = false;
                if ui.small_button("Копировать").clicked() {
                    copy = true;
                }
                if ui.small_button("Очистить").clicked() {
                    clear = true;
                }
                if copy {
                    let text = if is_input {
                        self.input.clone()
                    } else {
                        self.output.clone()
                    };
                    ui.ctx().copy_text(text);
                }
                if clear {
                    if is_input {
                        self.input.clear();
                    } else {
                        self.output.clear();
                        self.marks.clear();
                        self.journal.clear();
                    }
                }
            });
        });
        ui.separator();

        if chars > MAX_EDITOR_CHARS {
            ui.vertical_centered(|ui| {
                ui.add_space(24.0);
                ui.label(
                    egui::RichText::new(
                        "Текст слишком большой для экранного редактора.\n\
                         Обработайте его в файловом режиме:\n\n    yol -i большой.txt -o результат.txt",
                    )
                    .color(COLOR_MARK),
                );
            });
            return;
        }

        if !is_input {
            ui.checkbox(&mut self.highlight, "подсветка замен");
        }

        let highlighted = !is_input
            && self.highlight
            && self.opts.mark
            && !self.marks.is_empty()
            && self.output.len() <= MAX_HIGHLIGHT_CHARS;

        ui.push_id(if is_input { "scroll_in" } else { "scroll_out" }, |ui| {
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let width = if self.opts.word_wrap {
                        ui.available_width()
                    } else {
                        f32::INFINITY
                    };
                    if highlighted {
                        highlighted_label(ui, &self.output, &self.marks);
                    } else if is_input {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.input)
                                .font(egui::TextStyle::Monospace)
                                .desired_width(width)
                                .frame(false)
                                .hint_text(
                                    "Вставьте текст или загрузите файл (Файл → Загрузить текст…)",
                                ),
                        );
                    } else {
                        let r = ui.add(
                            egui::TextEdit::multiline(&mut self.output)
                                .font(egui::TextStyle::Monospace)
                                .desired_width(width)
                                .frame(false),
                        );
                        if r.changed() {
                            // Ручная правка результата делает пометки неверными.
                            self.marks.clear();
                        }
                    }
                });
        });
    }

    /// Панель запроса — прямая наследница `AskPanel` из Main.dfm.
    fn ask_panel(&mut self, ctx: &egui::Context) {
        let Some(ask) = self.ask.clone() else {
            return;
        };
        egui::TopBottomPanel::bottom("ask_panel")
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("Выберите вариант замены").strong());
                });
                ui.add_space(2.0);

                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new(ask.before.as_str())
                            .monospace()
                            .color(COLOR_DIM),
                    );

                    let label1 = if ask.focus_first {
                        format!("{} (1)", ask.word1)
                    } else {
                        ask.word1.clone()
                    };
                    let label2 = if ask.focus_first {
                        ask.word2.clone()
                    } else {
                        format!("{} (2)", ask.word2)
                    };
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new(label1)
                                .monospace()
                                .strong()
                                .color(COLOR_YE),
                        ))
                        .clicked()
                    {
                        self.answer(AskDecision::Ye);
                    }
                    ui.label(egui::RichText::new("/").monospace().color(COLOR_DIM));
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new(label2)
                                .monospace()
                                .strong()
                                .color(COLOR_YO),
                        ))
                        .clicked()
                    {
                        self.answer(AskDecision::Yo);
                    }
                    ui.label(
                        egui::RichText::new(ask.after.as_str())
                            .monospace()
                            .color(COLOR_DIM),
                    );
                });

                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Всегда Е (3)").clicked() {
                        self.answer(AskDecision::YeAll);
                    }
                    if ui.button("Всегда Ё (4)").clicked() {
                        self.answer(AskDecision::YoAll);
                    }
                    if ui.button("Откат").clicked() {
                        self.answer(AskDecision::Undo);
                    }
                    if ui.button("Прервать (Esc)").clicked() {
                        self.answer(AskDecision::Cancel);
                    }
                    if ask.wrong_yo {
                        ui.colored_label(
                            COLOR_MARK,
                            egui::RichText::new("подозрительная «ё»").small(),
                        );
                    } else if ask.replaced_yo {
                        ui.colored_label(
                            COLOR_MARK,
                            egui::RichText::new("«ё» стоит не там").small(),
                        );
                    }
                    if ask.wrong_ye {
                        ui.colored_label(
                            COLOR_YE,
                            egui::RichText::new("замена без запроса").small(),
                        );
                    }
                });

                // Горячие клавиши 1–4 и Esc.
                let mut decision: Option<AskDecision> = None;
                ui.input(|i| {
                    if i.key_pressed(egui::Key::Num1) {
                        decision = Some(AskDecision::Ye);
                    } else if i.key_pressed(egui::Key::Num2) {
                        decision = Some(AskDecision::Yo);
                    } else if i.key_pressed(egui::Key::Num3) {
                        decision = Some(AskDecision::YeAll);
                    } else if i.key_pressed(egui::Key::Num4) {
                        decision = Some(AskDecision::YoAll);
                    } else if i.key_pressed(egui::Key::Escape) {
                        decision = Some(AskDecision::Cancel);
                    }
                });
                if let Some(d) = decision {
                    self.answer(d);
                }
                ui.add_space(6.0);
            });
    }

    /// Строка состояния: слева — состояние, справа — статистика в формате
    /// строки состояния оригинала.
    fn status_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(self.status_left.as_str()).small());
                ui.separator();
                if self.busy() {
                    let done = self.stats.chars as f64;
                    let total = match &self.run {
                        Some(r) => (r.total_chars as f64).max(1.0),
                        None => done.max(1.0),
                    };
                    ui.add(
                        egui::ProgressBar::new((done / total) as f32)
                            .desired_width(130.0)
                            .show_percentage(),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(self.stats.classic())
                            .monospace()
                            .small()
                            .color(COLOR_DIM),
                    );
                });
            });
        });
    }
}

/// Строка таблицы «ключ — значение» в окне статистики.
fn stats_row(ui: &mut egui::Ui, key: &str, value: String) {
    ui.label(key);
    ui.label(egui::RichText::new(value).monospace());
    ui.end_row();
}

/// Следующий вариант политики для неоднозначных слов (кнопка-переключатель).
fn next_policy(p: AmbiguousPolicy) -> AmbiguousPolicy {
    match p {
        AmbiguousPolicy::Yo => AmbiguousPolicy::Ye,
        AmbiguousPolicy::Ye => AmbiguousPolicy::Ask,
        AmbiguousPolicy::Ask => AmbiguousPolicy::Yo,
    }
}

/// Рендер текста с подсветкой изменённых букв (через `LayoutJob`).
fn highlighted_label(ui: &mut egui::Ui, text: &str, marks: &[(usize, usize)]) {
    let normal = egui::TextFormat {
        font_id: egui::FontId::new(14.5, egui::FontFamily::Monospace),
        color: ui.visuals().text_color(),
        ..Default::default()
    };
    let mut hot = normal.clone();
    hot.color = COLOR_MARK;
    hot.background = COLOR_MARK_BG;

    let mut job = egui::text::LayoutJob::default();
    let mut pos = 0usize;
    for &(s, e) in marks {
        if s >= text.len() {
            break;
        }
        let (s, e) = (s.max(pos), e.min(text.len()));
        if e <= s || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
            continue;
        }
        if s > pos {
            job.append(&text[pos..s], 0.0, normal.clone());
        }
        job.append(&text[s..e], 0.0, hot.clone());
        pos = e;
    }
    if pos < text.len() {
        job.append(&text[pos..], 0.0, normal);
    }
    ui.label(job);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Окна
// ═══════════════════════════════════════════════════════════════════════════

impl YoApp {
    fn file_dialog_window(&mut self, ctx: &egui::Context) {
        if !self.dialog.open {
            return;
        }
        let mut open = true;
        let mut close = false;
        let title = if self.dialog.is_save {
            "Сохранить результат"
        } else {
            "Загрузить текст"
        };

        let mut action: Option<FileAction> = None;
        let mut new_dir: Option<PathBuf> = None;

        egui::Window::new(title)
            .open(&mut open)
            .resizable(true)
            .default_size([580.0, 420.0])
            .show(ctx, |ui| {
                // ── путь ───────────────────────────────────────────────
                ui.horizontal(|ui| {
                    ui.label("Папка:");
                    let mut dir_str = self.dialog.dir.to_string_lossy().to_string();
                    let r = ui.add_sized(
                        [ui.available_width() - 60.0, 20.0],
                        egui::TextEdit::singleline(&mut dir_str),
                    );
                    if r.lost_focus() {
                        new_dir = Some(PathBuf::from(dir_str));
                    }
                    if ui.button("↑").on_hover_text("На уровень выше").clicked() {
                        if let Some(p) = self.dialog.dir.parent() {
                            new_dir = Some(p.to_path_buf());
                        }
                    }
                });

                // ── имя и кодировка ────────────────────────────────────
                ui.horizontal(|ui| {
                    ui.label("Имя:");
                    ui.add_sized(
                        [ui.available_width() - 180.0, 20.0],
                        egui::TextEdit::singleline(&mut self.dialog.name),
                    );
                    ui.label("Кодировка:");
                    ui.menu_button(self.dialog.encoding.label(), |ui| {
                        for enc in TextEncoding::ALL {
                            ui.selectable_value(&mut self.dialog.encoding, enc, enc.label());
                        }
                    });
                });
                ui.separator();

                // ── список ─────────────────────────────────────────────
                let entries = self.dialog.entries.clone();
                ui.push_id("file_list", |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for (name, is_dir) in entries {
                                let label = if is_dir {
                                    format!("[ {} ]", name)
                                } else {
                                    name.clone()
                                };
                                let resp = ui.add(
                                    egui::Button::new(label)
                                        .frame(false)
                                        .fill(egui::Color32::TRANSPARENT),
                                );
                                if resp.double_clicked() {
                                    if is_dir {
                                        new_dir = Some(self.dialog.dir.join(&name));
                                    } else {
                                        action = Some(if self.dialog.is_save {
                                            FileAction::Save(
                                                self.dialog.dir.join(&name),
                                                self.dialog.encoding,
                                            )
                                        } else {
                                            FileAction::Load(
                                                self.dialog.dir.join(&name),
                                                self.dialog.encoding,
                                            )
                                        });
                                    }
                                } else if resp.clicked() {
                                    self.dialog.name = name.clone();
                                }
                            }
                        });
                });

                ui.separator();
                ui.horizontal(|ui| {
                    let ok = if self.dialog.is_save {
                        "Сохранить"
                    } else {
                        "Открыть"
                    };
                    if ui.button(ok).clicked() {
                        action = Some(if self.dialog.is_save {
                            FileAction::Save(self.dialog.path(), self.dialog.encoding)
                        } else {
                            FileAction::Load(self.dialog.path(), self.dialog.encoding)
                        });
                    }
                    if ui.button("Отмена").clicked() {
                        close = true;
                    }
                    if !self.dialog.status.is_empty() {
                        ui.colored_label(COLOR_MARK, self.dialog.status.as_str());
                    }
                });
            });

        // `open` снимается крестиком окна, `close` — кнопкой «Отмена».
        let mut open = open && !close;
        if let Some(d) = new_dir {
            self.dialog.dir = d;
            self.dialog.refresh();
        }
        if let Some(a) = action {
            match a {
                FileAction::Load(p, enc) => {
                    if p.is_file() {
                        self.load_path(p, Some(enc));
                        open = false;
                    } else {
                        self.dialog.status = "это не файл".to_owned();
                    }
                }
                FileAction::Save(p, enc) => {
                    if p.parent().map(|x| x.is_dir()).unwrap_or(false) {
                        self.save_path(p, enc);
                        open = false;
                    } else {
                        self.dialog.status = "нет такой папки".to_owned();
                    }
                }
            }
        }
        self.dialog.open = open;
    }

    fn options_window(&mut self, ctx: &egui::Context) {
        if !self.show_options {
            return;
        }
        let mut open = true;
        let mut close = false;
        egui::Window::new("Опции")
            .open(&mut open)
            .resizable(false)
            .default_size([540.0, 440.0])
            .show(ctx, |ui| {
                ui.heading("Ёфикация");
                let o = &mut self.opts;
                ui.checkbox(
                    &mut o.reg_exprs,
                    "Использование словаря регулярных выражений",
                );
                ui.checkbox(&mut o.no_var_only, "Только безопасные замены");
                ui.checkbox(&mut o.var_only, "Только интерактивные замены");
                ui.checkbox(&mut o.check_yo, "Проверять расстановку «ё»");
                ui.checkbox(&mut o.always_ask, "Всегда подтверждать замену");
                ui.checkbox(&mut o.to_confirm_abbr, "…подтверждать для сокращений");
                ui.checkbox(&mut o.to_confirm_cap, "…подтверждать для имён собственных");
                ui.checkbox(
                    &mut o.to_confirm_ellipsis,
                    "…подтверждать для оборванных слов",
                );
                ui.checkbox(&mut o.propose_last, "Предлагать последний вариант замены");
                ui.checkbox(&mut o.fb_format, "Учитывать формат «FictionBook»");
                ui.checkbox(&mut o.fast_scroll, "Быстрая прокрутка");
                ui.horizontal(|ui| {
                    ui.label("Количество строк проматки:");
                    let mut n = o.lines_below as i32;
                    ui.add(egui::DragValue::new(&mut n).range(0..=20));
                    o.lines_below = n.max(0) as usize;
                });
                ui.horizontal(|ui| {
                    ui.label("Неоднозначные слова:");
                    for p in AmbiguousPolicy::ALL {
                        ui.radio_value(&mut o.ambiguous, p, p.label());
                    }
                });

                ui.separator();
                ui.heading("Редактор");
                ui.checkbox(&mut o.mark, "Помечать исправленное");
                ui.checkbox(&mut o.word_wrap, "Переносить текст по словам");
                ui.checkbox(&mut o.auto_unicode, "Автораспознавание Unicode");
                ui.checkbox(&mut o.last_file, "Помнить последний файл");
                ui.checkbox(&mut self.highlight, "Подсвечивать замены в результате");
                ui.checkbox(&mut self.show_journal, "Вести журнал замен");

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("По умолчанию").clicked() {
                        self.opts = YoOptions::default();
                    }
                    if ui.button("Закрыть").clicked() {
                        close = true;
                    }
                });
            });
        self.show_options = open && !close;
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.show_about {
            return;
        }
        let mut open = true;
        let (words, dic_version, rules, re_version, ms) = match &self.dict_info {
            Some(v) => v.clone(),
            None => {
                let (w, dv, r, rv) = database_info();
                (w, dv, r, rv, 0u128)
            }
        };
        egui::Window::new("О программе")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.heading(egui::RichText::new("YOL — «Ё лайт»").strong());
                    ui.label(format!("версия {VERSION}"));
                });
                ui.separator();
                ui.label("Программа для ёфикации русских текстов.");
                ui.label("Полностью самодостаточный исполняемый файл: оба");
                ui.label("словаря встроены в бинарник и не читаются с диска.");
                ui.separator();
                egui::Grid::new("about_grid")
                    .num_columns(2)
                    .spacing([16.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("База слов:");
                        ui.label(format!("{words} слов, версия {dic_version}"));
                        ui.end_row();
                        ui.label("Регулярные выражения:");
                        ui.label(format!("{rules} правил, версия {re_version}"));
                        ui.end_row();
                        ui.label("Разбор баз:");
                        ui.label(format!("{ms} мс"));
                        ui.end_row();
                        ui.label("Интерфейс:");
                        ui.label("Rust + egui/eframe (WGPU / OpenGL)");
                        ui.end_row();
                        ui.label("Обработка:");
                        ui.label("в отдельном потоке, канал mpsc");
                        ui.end_row();
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Исходники:");
                    ui.hyperlink_to(
                        "github.com/templarundead/yo",
                        "https://github.com/templarundead/yo",
                    );
                });
            });
        self.show_about = open;
    }

    fn stats_window(&mut self, ctx: &egui::Context) {
        if !self.show_stats {
            return;
        }
        let mut open = true;
        let s = self.stats;
        egui::Window::new("Статистика")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                egui::Grid::new("stats_grid")
                    .num_columns(2)
                    .spacing([24.0, 4.0])
                    .show(ui, |ui| {
                        stats_row(ui, "Символов", s.chars.to_string());
                        stats_row(ui, "Слов", s.words.to_string());
                        stats_row(ui, "Проверено (есть е/ё)", s.checked.to_string());
                        stats_row(ui, "Заменено", s.checked_changed.to_string());
                        stats_row(ui, "Оставлено как есть", s.checked_not_changed.to_string());
                        stats_row(ui, "е → ё", s.e_to_yo.to_string());
                        stats_row(ui, "ё → е", s.yo_to_e.to_string());
                        stats_row(ui, "Неоднозначных", s.asked.to_string());
                        stats_row(ui, "Сработало рег. выражений", s.re.to_string());
                        stats_row(ui, "…из них с заменой", s.re_changed.to_string());
                        stats_row(ui, "…без результата", s.no_re.to_string());
                        stats_row(ui, "Качество РВ (REQ)", format!("{:.1} %", s.re_quality()));
                        stats_row(ui, "Подозрительных «ё»", s.wrong_yo.to_string());
                        stats_row(ui, "Время", format!("{:.3} с", s.elapsed.as_secs_f64()));
                        stats_row(ui, "Скорость", format!("{:.0} зам/с", s.speed()));
                    });
                ui.separator();
                ui.label(
                    egui::RichText::new(s.classic())
                        .monospace()
                        .small()
                        .color(COLOR_DIM),
                );
            });
        self.show_stats = open;
    }

    fn journal_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_journal;
        egui::Window::new("Журнал замен")
            .open(&mut open)
            .resizable(true)
            .default_size([420.0, 240.0])
            .show(ctx, |ui| {
                ui.push_id("journal_scroll", |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if self.journal.is_empty() {
                                ui.label(egui::RichText::new("пока пусто").color(COLOR_DIM));
                            }
                            let n = self.journal.len();
                            for i in (0..n).rev() {
                                let (from, to) = &self.journal[i];
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(from.as_str())
                                            .monospace()
                                            .color(COLOR_YE),
                                    );
                                    ui.label(egui::RichText::new("→").monospace());
                                    ui.label(
                                        egui::RichText::new(to.as_str())
                                            .monospace()
                                            .color(COLOR_YO),
                                    );
                                });
                            }
                        });
                });
            });
        self.show_journal = open;
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Консольный режим
// ═══════════════════════════════════════════════════════════════════════════

struct Args {
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    in_enc: Option<TextEncoding>,
    out_enc: Option<TextEncoding>,
    gui: bool,
    quiet: bool,
    opts: YoOptions,
    help: bool,
    version: bool,
    error: Option<String>,
}

impl Args {
    fn parse(argv: impl Iterator<Item = String>) -> Self {
        let mut a = Args {
            input: None,
            output: None,
            in_enc: None,
            out_enc: None,
            gui: false,
            quiet: false,
            opts: YoOptions::default(),
            help: false,
            version: false,
            error: None,
        };
        let mut it = argv.skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "-h" | "--help" => a.help = true,
                "-V" | "--version" => a.version = true,
                "-g" | "--gui" => a.gui = true,
                "-q" | "--quiet" => a.quiet = true,
                "-i" | "--input" => match it.next() {
                    Some(v) => a.input = Some(PathBuf::from(v)),
                    None => a.error = Some("не указан файл после --input".into()),
                },
                "-o" | "--output" => match it.next() {
                    Some(v) => a.output = Some(PathBuf::from(v)),
                    None => a.error = Some("не указан файл после --output".into()),
                },
                "--in-enc" => match it.next().and_then(parse_encoding) {
                    Some(e) => a.in_enc = Some(e),
                    None => a.error = Some("неизвестная кодировка (--in-enc)".into()),
                },
                "--out-enc" => match it.next().and_then(parse_encoding) {
                    Some(e) => a.out_enc = Some(e),
                    None => a.error = Some("неизвестная кодировка (--out-enc)".into()),
                },
                "--no-regexpr" => a.opts.reg_exprs = false,
                "--regexpr" => a.opts.reg_exprs = true,
                "--safe" => a.opts.no_var_only = true,
                "--interactive-only" => a.opts.var_only = true,
                "--no-check-yo" => a.opts.check_yo = false,
                "--always-ask" => a.opts.always_ask = true,
                "--confirm-abbr" => a.opts.to_confirm_abbr = true,
                "--no-confirm-abbr" => a.opts.to_confirm_abbr = false,
                "--confirm-cap" => a.opts.to_confirm_cap = true,
                "--confirm-ellipsis" => a.opts.to_confirm_ellipsis = true,
                "--no-confirm-ellipsis" => a.opts.to_confirm_ellipsis = false,
                "--no-propose-last" => a.opts.propose_last = false,
                "--fb2" => a.opts.fb_format = true,
                "--no-mark" => a.opts.mark = false,
                "--word-wrap" => a.opts.word_wrap = true,
                "--ambiguous" => match it.next() {
                    Some(v) => match v.as_str() {
                        "yo" | "ё" => a.opts.ambiguous = AmbiguousPolicy::Yo,
                        "ye" | "е" => a.opts.ambiguous = AmbiguousPolicy::Ye,
                        "ask" => a.opts.ambiguous = AmbiguousPolicy::Ask,
                        other => a.error = Some(format!("неизвестная политика: {other}")),
                    },
                    None => a.error = Some("не указано значение --ambiguous".into()),
                },
                "-" => {
                    if a.input.is_none() {
                        a.input = Some(PathBuf::from("-"));
                    } else {
                        a.output = Some(PathBuf::from("-"));
                    }
                }
                other => {
                    if other.starts_with('-') {
                        a.error = Some(format!("неизвестный ключ: {other}"));
                    } else if a.input.is_none() {
                        a.input = Some(PathBuf::from(other));
                    } else {
                        a.output = Some(PathBuf::from(other));
                    }
                }
            }
        }
        a
    }

    /// Консольный режим: задан файл ввода/вывода, явно указан `-`
    /// (stdin/stdout) либо stdin перенаправлен — `cat файл.txt | yol`.
    ///
    /// Оговорка: у GUI-приложения, запущенного из .desktop-файла, stdin
    /// обычно тоже не терминал, поэтому такой запуск нужно помечать `-g`.
    fn is_cli(&self) -> bool {
        !self.help
            && !self.version
            && !self.gui
            && (self.input.is_some() || self.output.is_some() || !std::io::stdin().is_terminal())
    }
}

fn parse_encoding(s: String) -> Option<TextEncoding> {
    let v = s.to_ascii_lowercase().replace(['-', '_'], "");
    match v.as_str() {
        "utf8" | "u8" => Some(TextEncoding::Utf8),
        "utf8bom" | "bom" => Some(TextEncoding::Utf8Bom),
        "utf16" | "utf16le" | "u16" => Some(TextEncoding::Utf16Le),
        "utf16be" => Some(TextEncoding::Utf16Be),
        "cp1251" | "windows1251" | "win1251" | "ansi" => Some(TextEncoding::Cp1251),
        _ => None,
    }
}

/// [`Sink`], пишущий результат в поток с нужной кодировкой на лету —
/// без промежуточного хранения текста в памяти.
struct EncodingSink<W: std::io::Write> {
    writer: W,
    enc: TextEncoding,
    first: bool,
    quiet: bool,
    last: Instant,
}

impl<W: std::io::Write> EncodingSink<W> {
    fn new(writer: W, enc: TextEncoding, quiet: bool) -> Self {
        Self {
            writer,
            enc,
            first: true,
            quiet,
            last: Instant::now(),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut self.writer)
    }
}

impl<W: std::io::Write> Sink for EncodingSink<W> {
    fn chunk(&mut self, _base: usize, text: &str, _marks: &[(usize, usize)]) {
        // BOM пишем ровно один раз — в первой порции.
        let enc = if self.first {
            self.first = false;
            self.enc
        } else {
            match self.enc {
                TextEncoding::Utf8Bom => TextEncoding::Utf8,
                other => other,
            }
        };
        let _ = self.writer.write_all(&encode_string(text, enc));
    }

    fn stats(&mut self, stats: Stats) {
        if self.quiet {
            return;
        }
        let now = Instant::now();
        if now.duration_since(self.last).as_millis() < 200 {
            return;
        }
        self.last = now;
        eprint!("\r\x1b[2K  {}", stats.classic());
    }

    fn ask(&mut self, _req: AskRequest) -> AskDecision {
        // В консоли подтверждать некого — берём вариант с «Ё».
        AskDecision::Yo
    }

    fn undo(&mut self, _offset: usize, _old: &str, _new: &str) {}

    fn is_cancelled(&self) -> bool {
        false
    }
}

/// `-` в качестве имени файла означает stdin (на входе) или stdout (на выходе).
fn is_dash(p: &Path) -> bool {
    p.as_os_str() == "-"
}

fn run_cli(args: Args) -> i32 {
    let from_stdin = args.input.as_deref().map(is_dash).unwrap_or(true);

    let data: Vec<u8> = if from_stdin {
        use std::io::Read;
        let mut buf = Vec::new();
        if let Err(e) = std::io::stdin().read_to_end(&mut buf) {
            eprintln!("yol: не удалось прочитать stdin: {e}");
            return 1;
        }
        buf
    } else {
        let path = args.input.as_ref().expect("from_stdin == false");
        match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("yol: не удалось прочитать {}: {e}", path.display());
                return 1;
            }
        }
    };

    let (text, detected) = match args.in_enc {
        Some(e) => (decode_with(&data, e), e),
        None => decode_bytes(&data),
    };

    if !args.quiet {
        eprintln!(
            "yol: {} символов, кодировка {}",
            text.chars().count(),
            detected.label()
        );
    }

    let out_enc = args.out_enc.unwrap_or(match detected {
        TextEncoding::Cp1251 => TextEncoding::Cp1251,
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => detected,
        _ => TextEncoding::Utf8Bom,
    });

    let to_stdout = args.output.as_deref().map(is_dash).unwrap_or(true);

    let mut engine = Engine::new(args.opts);
    let t0 = Instant::now();
    let stats = match (&args.output, to_stdout) {
        (_, true) => {
            let stdout = std::io::stdout();
            let mut sink = EncodingSink::new(
                std::io::BufWriter::with_capacity(1 << 20, stdout.lock()),
                out_enc,
                args.quiet,
            );
            let s = engine.process(&text, &mut sink);
            let _ = sink.flush();
            s
        }
        (Some(path), false) => match std::fs::File::create(path) {
            Ok(f) => {
                let mut sink = EncodingSink::new(
                    std::io::BufWriter::with_capacity(1 << 20, f),
                    out_enc,
                    args.quiet,
                );
                let s = engine.process(&text, &mut sink);
                if let Err(e) = sink.flush() {
                    eprintln!("yol: не удалось записать {}: {e}", path.display());
                    return 1;
                }
                s
            }
            Err(e) => {
                eprintln!("yol: не удалось создать {}: {e}", path.display());
                return 1;
            }
        },
        (None, false) => unreachable!("to_stdout == false влечёт Some(path)"),
    };
    let wall = t0.elapsed();

    if !args.quiet {
        eprintln!("\r\x1b[2K  {}", stats.classic());
        eprintln!(
            "yol: готово за {:.2} с · {} замен · {}",
            wall.as_secs_f64(),
            stats.checked_changed,
            if to_stdout {
                format!("выведено в stdout ({})", out_enc.label())
            } else {
                let p = args.output.as_deref().unwrap_or(Path::new("-"));
                format!("записано в {} ({})", p.display(), out_enc.label())
            }
        );
    }
    0
}

const HELP: &str = "\
YOL — «Ё лайт»: ёфикация русских текстов

ИСПОЛЬЗОВАНИЕ
    yol                            графический интерфейс
    yol -i файл.txt                результат в stdout
    yol -i файл.txt -o out.txt     файл → файл (потоково, любые объёмы)
    cat файл.txt | yol             чтение из stdin
    cat файл.txt | yol - -o out    то же явно: «-» = stdin/stdout

КЛЮЧИ
    -i, --input <файл>        исходный текст («-» — stdin)
    -o, --output <файл>       куда писать результат («-» — stdout)
        --in-enc <кодировка>  utf8 | utf8bom | utf16 | utf16be | cp1251 (иначе авто)
        --out-enc <кодировка> кодировка результата
    -g, --gui                 принудительно открыть графический интерфейс
                              (нужен при запуске из .desktop, где stdin не терминал)
    -q, --quiet               не печатать прогресс
    -h, --help                эта справка
    -V, --version             версия

НАСТРОЙКИ ЁФИКАЦИИ
        --no-regexpr           не использовать словарь регулярных выражений
        --safe                 только безопасные (бесспорные) замены
        --interactive-only     только интерактивные замены
        --no-check-yo          не проверять расстановку «ё»
        --always-ask           всегда подтверждать замену
        --confirm-abbr         подтверждать замену для сокращений
        --confirm-cap          подтверждать замену для имён собственных
        --confirm-ellipsis     подтверждать замену для оборванных слов
        --no-propose-last      не предлагать последний вариант
        --fb2                  учитывать формат FictionBook
        --no-mark              не помечать исправленное
        --ambiguous <политика> что делать с неоднозначными словами:
                               yo (ставить Ё, по умолчанию) | ye (оставить Е) | ask
";

// ═══════════════════════════════════════════════════════════════════════════
//  Точка входа
// ═══════════════════════════════════════════════════════════════════════════

/// Рендерер по умолчанию: WGPU, если он собран, иначе OpenGL.
///
/// Сборка без WGPU (`cargo build --no-default-features`) даёт бинарник
/// примерно на 4 МБ меньше — на машинах без Vulkan это самый разумный выбор.
#[cfg(feature = "renderer-wgpu")]
fn default_renderer() -> eframe::Renderer {
    eframe::Renderer::Wgpu
}

#[cfg(not(feature = "renderer-wgpu"))]
fn default_renderer() -> eframe::Renderer {
    eframe::Renderer::Glow
}

fn main() -> eframe::Result<()> {
    let args = Args::parse(std::env::args());

    if let Some(err) = &args.error {
        eprintln!("yol: {err}");
        eprint!("{HELP}");
        std::process::exit(2);
    }
    if args.version {
        println!("yol {VERSION}");
        return Ok(());
    }
    if args.help {
        print!("{HELP}");
        return Ok(());
    }
    if args.is_cli() {
        std::process::exit(run_cli(args));
    }

    // WGPU по умолчанию; на ARM-платах без Vulkan удобно переключиться:
    //   YOL_RENDERER=glow ./yol
    let renderer = match std::env::var("YOL_RENDERER").as_deref() {
        Ok("glow") | Ok("gl") | Ok("opengl") => eframe::Renderer::Glow,
        _ => default_renderer(),
    };

    let options = eframe::NativeOptions {
        renderer,
        // Аппаратное ускорение включаем всегда (WGPU/Vulkan/Metal или OpenGL);
        // без GPU — автоматический откат на программный растеризатор.
        hardware_acceleration: eframe::HardwareAcceleration::Preferred,
        multisampling: 4,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title(APP_NAME),
        ..Default::default()
    };

    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| Ok(Box::new(YoApp::new(cc)))),
    )
}
