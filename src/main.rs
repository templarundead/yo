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
//!   │    ├ apply_chunk(): text.truncate(base)      │       │   через GuiSink   │
//!   │    ├ рисует панель запроса                   │       │   (стоит на ответе)│
//!   │    └ ans_tx.send(AskDecision) ───────────────┼──────►│                   │
//!   └──────────────────────────────────────────────┘       └───────────────────┘
//! ```
//!
//! Текст в окне один — как единственный `TRichEdit` Delphi-оригинала. Движок
//! обрабатывает снимок буфера, а интерфейс переписывает этот же `String` «на
//! месте»: `text.truncate(base)`, затем порция результата и ещё не обработанный
//! хвост исходника. Поэтому пользователь видит, как буквы меняются прямо в
//! тексте, а не в отдельной панели «результат».
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

use egui::text::{CCursor, CCursorRange};
use egui::widgets::text_edit::TextEditState;

// Ядро живёт в библиотеке `yol` (src/lib.rs → src/yo_core.rs), а не подключается
// модулем: так его же можно покрывать интеграционными тестами из tests/.
use yol::yo_core::{
    clear_user_selections, database_info, decode_bytes, decode_cp1251, encode_string, preload,
    utf16_units, AmbiguousPolicy, AskDecision, AskRequest, Engine, Sink, Stats, TextEncoding,
    YoOptions,
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
/// «Тревожный» заголовок панели запроса (в оригинале — `clRed`).
const COLOR_ALERT: egui::Color32 = egui::Color32::from_rgb(255, 120, 120);

/// Плашка под спорным словом: полупрозрачная, чтобы буквы оставались видны.
const COLOR_ASK_BG: egui::Color32 = egui::Color32::from_rgba_premultiplied(70, 52, 10, 70);

/// Постоянный id единого редактора: по нему `TextEditState` находит выделение,
/// которым GUI отмечает спорное слово — аналог `Editor.SelStart/SelLength`.
fn text_edit_id() -> egui::Id {
    egui::Id::new("yol_main_text")
}

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

/// Переписать единый буфер «на месте»: `base` — длина готового префикса.
///
/// Ровно то, что делал оригинал со своим единственным `RichEdit`: хвост после
/// префикса отбрасывается, вместо него кладётся новая порция, а затем
/// возвращается ещё не обработанный остаток исходника (`source`). Длина
/// буфера не меняется — движок меняет только е↔ё, — поэтому курсор,
/// прокрутка и пометки остаются на месте.
fn rewrite_in_place(buffer: &mut String, source: Option<&str>, base: usize, chunk: &str) {
    if base > buffer.len() || !buffer.is_char_boundary(base) {
        return;
    }
    buffer.truncate(base);
    buffer.push_str(chunk);
    if let Some(source) = source {
        let from = (base + chunk.len()).min(source.len());
        if source.is_char_boundary(from) {
            buffer.push_str(&source[from..]);
        }
    }
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
            String::from_utf16_lossy(&utf16_units(&bytes[skip..], le))
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Приложение
// ═══════════════════════════════════════════════════════════════════════════

struct YoApp {
    // ── данные ──────────────────────────────────────────────────────────
    opts: YoOptions,
    /// Единственный буфер «текст ↔ результат», как единственный RichEdit
    /// оригинала. Ёфикация переписывает его прямо на месте.
    text: String,
    /// Длина `text` в символах (кэш: `chars().count()` на каждый кадр дорог).
    text_chars: usize,
    /// Снимок исходника на время прогона: из него после каждой порции
    /// возвращается ещё не обработанный хвост. Хранится в `Arc`, потому что
    /// тот же буфер читает рабочий поток.
    run_source: Option<Arc<String>>,
    marks: Vec<(usize, usize)>,
    /// Журнал замен («было → стало»).
    journal: Vec<(String, String)>,

    // ── состояние ───────────────────────────────────────────────────────
    run: Option<Run>,
    ask: Option<AskRequest>,
    /// Спорное слово в индексах **символов** — то, что выделяет редактор.
    ask_selection: Option<(usize, usize)>,
    /// Куда поставить курсор после ответа (`SelLength := 0` оригинала).
    selection_after: Option<usize>,
    /// Прокрутить редактор к спорному слову на ближайшем кадре.
    scroll_to_ask: bool,
    stats: Stats,
    dict_info: Option<(usize, String, usize, String, u128)>,
    status_left: String,

    // ── интерфейс ───────────────────────────────────────────────────────
    dark: bool,
    /// Какая тема уже применена к стилю (`None` — ещё ни одна).
    applied_dark: Option<bool>,
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
        Self::with_context(&cc.egui_ctx)
    }

    /// Сборка приложения с готовым `egui::Context`: так `YoApp` можно поднять
    /// и в тестах, где окна нет вовсе.
    fn with_context(ctx: &egui::Context) -> Self {
        // Тёмная тема — сразу и по умолчанию.
        setup_fonts(ctx);
        apply_theme(ctx, true);

        // Словари (1.8 МБ) разбираем в фоне: окно появляется мгновенно,
        // а первый прогон всё равно дождётся готовности базы.
        std::thread::spawn(preload);

        Self {
            opts: YoOptions::default(),
            text: String::new(),
            text_chars: 0,
            run_source: None,
            marks: Vec::new(),
            journal: Vec::new(),
            run: None,
            ask: None,
            ask_selection: None,
            selection_after: None,
            scroll_to_ask: false,
            stats: Stats::default(),
            dict_info: None,
            status_left: "готов — вставьте текст или загрузите файл".to_owned(),
            dark: true,
            applied_dark: None,
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
        if self.text.is_empty() {
            self.status_left = "нечего ёфицировать".to_owned();
            return;
        }

        let (tx, rx) = mpsc::channel::<Event>();
        let (ans_tx, ans_rx) = mpsc::channel::<AskDecision>();
        let cancel = Arc::new(AtomicBool::new(false));

        // Движок работает со снимком: буфер интерфейса тем временем
        // переписывается «на месте» теми же смещениями.
        let input = Arc::new(self.text.clone());
        let engine_input = Arc::clone(&input);
        let opts = self.opts.clone();
        let want_journal = self.show_journal;
        let cancel_thread = cancel.clone();
        let total_chars = self.text_chars;

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
            let stats = engine.process(engine_input.as_str(), &mut sink);
            let _ = tx.send(Event::Done { stats });
        });

        self.run_source = Some(input);
        self.marks.clear();
        self.journal.clear();
        self.stats = Stats::default();
        self.ask = None;
        self.ask_selection = None;
        self.selection_after = None;
        self.scroll_to_ask = false;
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
        self.scroll_to_ask = false;
        if let Some((start, _)) = self.ask_selection.take() {
            self.selection_after = Some(start);
        }
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
                    self.apply_chunk(base, &text, &marks);
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
                Ok(Event::Ask(req)) => {
                    // Куда поставить выделение в едином буфере. Оригинал
                    // делал ровно это: `Editor.SelStart := Idx`.
                    self.ask_selection = req.char_range(&self.text);
                    self.scroll_to_ask = self.ask_selection.is_some();
                    self.ask = Some(req);
                }
                Ok(Event::Undo { offset, old, new }) => self.patch_text(offset, &old, &new),
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
            let cancelled = run.cancel.load(Ordering::Relaxed);
            self.ask = None;
            self.scroll_to_ask = false;
            // Если ответ ещё не успел снять выделение — снимем его на
            // ближайшем кадре редактора, иначе `selection_after` пропадёт.
            if let Some((start, _)) = self.ask_selection.take() {
                self.selection_after = Some(start);
            }
            // Остаток исходника больше не нужен: буфер — это результат.
            self.run_source = None;
            let changed = self.stats.checked_changed;
            self.status_left = if cancelled {
                format!("прервано · заменено {changed}")
            } else {
                format!("готово · заменено {changed}")
            };
        } else {
            self.run = Some(run);
        }
    }

    /// Применить порцию результата к единому буферу «на месте».
    ///
    /// `base` — длина уже готового префикса. Ровно как в оригинале с его
    /// единственным RichEdit: хвост после префикса отбрасывается
    /// (`truncate`), вместо него кладётся новая порция, а затем возвращается
    /// ещё не обработанный остаток исходника. Длина буфера не меняется, зато
    /// пользователь видит, как буквы исправляются прямо в тексте.
    fn apply_chunk(&mut self, base: usize, chunk: &str, marks: &[(usize, usize)]) {
        if base > self.text.len() || !self.text.is_char_boundary(base) {
            return;
        }
        if self.marks.len() < MAX_GUI_MARKS {
            self.marks.reserve(marks.len());
            for &(s, e) in marks {
                self.marks.push((base + s, base + e));
            }
        }
        let source = self.run_source.as_deref().map(String::as_str);
        rewrite_in_place(&mut self.text, source, base, chunk);
    }

    /// Откатить уже показанную замену прямо в едином буфере.
    fn patch_text(&mut self, offset: usize, old: &str, new: &str) {
        let end = match offset.checked_add(new.len()) {
            Some(e) if e <= self.text.len() => e,
            _ => return,
        };
        if !self.text.is_char_boundary(offset) || !self.text.is_char_boundary(end) {
            return;
        }
        if &self.text[offset..end] != new {
            return;
        }
        self.text.replace_range(offset..end, old);
        // Откат — это е↔ё, длина не меняется; на всякий случай поддерживаем
        // и изменение длины, чтобы метки и счётчик остались верными.
        let delta = old.len() as i64 - new.len() as i64;
        if delta != 0 {
            for (s, e) in self.marks.iter_mut() {
                if *s >= end {
                    *s = (*s as i64 + delta) as usize;
                    *e = (*e as i64 + delta) as usize;
                }
            }
            self.text_chars = self.text.chars().count();
        }
        self.marks.retain(|&(s, _)| !(s >= offset && s < end));
    }

    /// Ответить рабочему потоку на панель запроса.
    fn answer(&mut self, decision: AskDecision) {
        let tx = match &self.run {
            Some(r) => r.ans_tx.clone(),
            None => return,
        };
        if self.ask.is_none() {
            // Вопрос уже снят (кнопку нажали в этом же кадре, а следом
            // прилетела горячая клавиша) — второй ответ движку не нужен.
            return;
        }
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
        // Оригинал после ответа делал `Editor.SelLength := 0`: каретка
        // остаётся в начале слова, выделение снимается на ближайшем кадре.
        if let Some((start, _)) = self.ask_selection.take() {
            self.selection_after = Some(start);
        }
        let _ = tx.send(decision);
    }

    // ────────────────────────────────────────────────────────────────────
    //  Файлы
    // ────────────────────────────────────────────────────────────────────

    fn load_path(&mut self, path: PathBuf, enc: Option<TextEncoding>) {
        if self.busy() {
            self.status_left = "дождитесь окончания ёфикации".to_owned();
            return;
        }
        match std::fs::read(&path) {
            Ok(bytes) => {
                let (text, detected) = match enc {
                    Some(e) => (decode_with(&bytes, e), e),
                    None => decode_bytes(&bytes),
                };
                let n = text.chars().count();
                self.text = text;
                self.text_chars = n;
                self.run_source = None;
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
        let bytes = encode_string(&self.text, enc);
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
            // Esc без вопроса — «Прервать», как в оригинале.
            if self.ask.is_none() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.stop();
            }
        }

        self.menu_bar(ctx);
        self.options_strip(ctx);
        // Нижние панели накладываются в порядке вызова: первая — самая нижняя.
        self.status_bar(ctx);
        if self.ask.is_some() {
            self.ask_panel(ctx);
        }
        self.editor_panel(ctx);

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
                            !self.busy() && !self.text.is_empty(),
                            egui::Button::new("Сохранить текст…"),
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
                    // Ctrl+X / Ctrl+C / Ctrl+V обслуживает сам `TextEdit`
                    // (нативные события egui). Пунктам меню достаточно
                    // попросить об этом eframe: `RequestPaste` заставляет
                    // интеграцию прочитать системный буфер обмена и прислать
                    // `Event::Paste` — отдельный `arboard` не нужен.
                    let idle = !self.busy();
                    let can_cut = idle && self.has_selection(ctx);
                    if ui
                        .add_enabled(can_cut, egui::Button::new("Вырезать"))
                        .on_hover_text("Ctrl+X")
                        .clicked()
                    {
                        ctx.memory_mut(|m| m.request_focus(text_edit_id()));
                        ctx.send_viewport_cmd(egui::ViewportCommand::RequestCut);
                        ui.close_menu();
                    }
                    if ui
                        .add_enabled(can_cut, egui::Button::new("Копировать"))
                        .on_hover_text("Ctrl+C")
                        .clicked()
                    {
                        ctx.memory_mut(|m| m.request_focus(text_edit_id()));
                        ctx.send_viewport_cmd(egui::ViewportCommand::RequestCopy);
                        ui.close_menu();
                    }
                    if ui
                        .add_enabled(idle, egui::Button::new("Вставить"))
                        .on_hover_text("Ctrl+V")
                        .clicked()
                    {
                        ctx.memory_mut(|m| m.request_focus(text_edit_id()));
                        ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(idle, egui::Button::new("Выделить всё"))
                        .on_hover_text("Ctrl+A")
                        .clicked()
                    {
                        self.select_all(ctx);
                        ui.close_menu();
                    }
                    if ui
                        .add_enabled(
                            !self.text.is_empty(),
                            egui::Button::new("Копировать весь текст"),
                        )
                        .clicked()
                    {
                        ctx.copy_text(self.text.clone());
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            idle && !self.text.is_empty(),
                            egui::Button::new("Очистить всё"),
                        )
                        .clicked()
                    {
                        self.text.clear();
                        self.text_chars = 0;
                        self.marks.clear();
                        self.journal.clear();
                        self.ask_selection = None;
                        self.selection_after = None;
                        ui.close_menu();
                    }
                });

                ui.menu_button("Ёфикация", |ui| {
                    if ui
                        .add_enabled(
                            !self.busy() && !self.text.is_empty(),
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

    /// Единственный редактор: текст и результат — один и тот же буфер.
    fn editor_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Текст").strong());
                ui.label(
                    egui::RichText::new(format!("{} симв.", self.text_chars))
                        .small()
                        .color(COLOR_DIM),
                );
                if self.busy() {
                    ui.spinner();
                    ui.label(
                        egui::RichText::new("ёфикация…")
                            .small()
                            .color(COLOR_DIM),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.checkbox(&mut self.highlight, "подсветка замен")
                        .on_hover_text("Красить буквы, которые изменила ёфикация");
                });
            });
            ui.separator();

            if self.text_chars > MAX_EDITOR_CHARS {
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

            // Пока идёт прогон, движок переписывает этот же буфер, поэтому
            // ввод блокируем — как `Editor.ReadOnly := True` оригинала.
            let busy = self.busy();
            self.sync_editor_selection(ctx);
            let ask_word = self.ask_selection;
            let scroll_now = self.scroll_to_ask && ask_word.is_some();

            let highlighted = self.highlight
                && self.opts.mark
                && !self.marks.is_empty()
                && self.text.len() <= MAX_HIGHLIGHT_CHARS;

            let marks: &[(usize, usize)] = &self.marks;
            let mut layouter = |ui: &egui::Ui, s: &str, wrap_width: f32| {
                let mut job = highlight_job(ui, s, marks);
                job.wrap.max_width = wrap_width;
                ui.fonts(|f| f.layout_job(job))
            };

            let row_height = ui.text_style_height(&egui::TextStyle::Monospace).max(1.0);
            let rows = ((ui.available_height() / row_height).floor() as usize).max(1);
            let width = if self.opts.word_wrap {
                ui.available_width()
            } else {
                f32::INFINITY
            };

            let mut changed = false;
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let mut edit = egui::TextEdit::multiline(&mut self.text)
                        .id(text_edit_id())
                        .font(egui::TextStyle::Monospace)
                        .desired_width(width)
                        .desired_rows(rows)
                        .frame(false)
                        .interactive(!busy)
                        .hint_text(
                            "Вставьте текст или загрузите файл (Файл → Загрузить текст…)",
                        );
                    if highlighted {
                        edit = edit.layouter(&mut layouter);
                    }
                    let out = edit.show(ui);
                    changed = out.response.changed();
                    if let Some(range) = ask_word {
                        // `TextEdit` рисует выделение только в фокусе, а
                        // read-only редактор его не получает — подсвечиваем
                        // слово сами, как `Editor.SelLength` оригинала.
                        paint_ask_word(ui, &out, range);
                        if scroll_now {
                            // Оригинал прокручивал редактор к спорному слову.
                            let cursor = out.galley.from_ccursor(CCursor::new(range.0));
                            let rect = out
                                .galley
                                .pos_from_cursor(&cursor)
                                .translate(out.galley_pos.to_vec2());
                            ui.scroll_to_rect(rect, Some(egui::Align::Center));
                        }
                    }
                });
            if scroll_now {
                self.scroll_to_ask = false;
            }

            if changed {
                // Ручная правка делает пометки неверными.
                self.marks.clear();
                self.text_chars = self.text.chars().count();
            }
        });
    }

    /// Отметить спорное слово в состоянии редактора — аналог
    /// `Editor.SelStart := Idx; Editor.SelLength := Length(Word1)`.
    ///
    /// Состояние `TextEditState` — единственный «официальный» способ задать
    /// выделение снаружи; видимую плашку рисует [`paint_ask_word`], потому что
    /// редактор во время прогона только для чтения и фокуса не получает.
    fn sync_editor_selection(&mut self, ctx: &egui::Context) {
        let id = text_edit_id();
        if let Some((start, end)) = self.ask_selection {
            let mut state = TextEditState::load(ctx, id).unwrap_or_default();
            state.cursor.set_char_range(Some(CCursorRange::two(
                CCursor::new(start),
                CCursor::new(end),
            )));
            state.store(ctx, id);
            return;
        }
        if let Some(pos) = self.selection_after.take() {
            // Ответ получен — снимаем выделение (`SelLength := 0`).
            let mut state = TextEditState::load(ctx, id).unwrap_or_default();
            state
                .cursor
                .set_char_range(Some(CCursorRange::one(CCursor::new(pos))));
            state.store(ctx, id);
        }
    }

    /// Есть ли в редакторе непустое выделение (для пунктов «Вырезать»/«Копировать»).
    fn has_selection(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|mem| mem.has_focus(text_edit_id()))
            && TextEditState::load(ctx, text_edit_id())
                .and_then(|s| s.cursor.char_range())
                .is_some_and(|r| r.primary.index != r.secondary.index)
    }

    /// Выделить весь текст (Ctrl+A / «Правка → Выделить всё»).
    fn select_all(&mut self, ctx: &egui::Context) {
        let n = self.text.chars().count();
        self.text_chars = n;
        let id = text_edit_id();
        let mut state = TextEditState::load(ctx, id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(n))));
        state.store(ctx, id);
        ctx.memory_mut(|mem| mem.request_focus(id));
    }

    /// Панель запроса — наследница `AskPanel` из Main.dfm.
    ///
    /// Рамка с заголовком и шесть кнопок в один ряд ровно по центру окна:
    /// `[вариант 1] [вариант 2] [Всегда Е (3)] [Всегда Ё (4)] [Откат]
    /// [Прервать (Esc)]`.
    fn ask_panel(&mut self, ctx: &egui::Context) {
        let Some(ask) = self.ask.clone() else {
            return;
        };

        let mut decision: Option<AskDecision> = None;
        egui::TopBottomPanel::bottom("ask_panel")
            .resizable(false)
            // Высота запоминается с предыдущего кадра; запас на первый кадр,
            // чтобы кнопки не оказались за нижним краем окна.
            .default_height(88.0)
            .show(ctx, |ui| {
                let (clicked, _) = ask_panel_ui(ui, &ask);
                decision = clicked;
            });

        // Горячие клавиши: 1–4, Enter (вариант в фокусе, как bYe/bYo
        // оригинала) и Esc.
        if decision.is_none() {
            ctx.input(|i| {
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
                } else if i.key_pressed(egui::Key::Enter) {
                    // В оригинале фокус стоял на bYe или bYo, и Enter нажимал её.
                    decision = Some(if ask.focus_first {
                        AskDecision::Ye
                    } else {
                        AskDecision::Yo
                    });
                }
            });
        }
        if let Some(d) = decision {
            self.answer(d);
        }
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

/// `LayoutJob` для единого редактора: обычный моноширинный текст плюс
/// подсветка изменённых букв.
///
/// Текст обязан остаться ровно тем же, что и в буфере (никаких вставок),
/// иначе `TextEdit` потеряет соответствие курсора и строк. Шрифт и цвет
/// повторяют умолчания `TextEdit`, чтобы включение подсветки не «дёргало»
/// вёрстку.
fn highlight_job(ui: &egui::Ui, text: &str, marks: &[(usize, usize)]) -> egui::text::LayoutJob {
    let normal = egui::TextFormat {
        font_id: egui::FontId::new(14.5, egui::FontFamily::Monospace),
        // Тот же цвет, что берёт сам `TextEdit` по умолчанию.
        color: ui.visuals().widgets.inactive.text_color(),
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
    job
}

/// Подсветить спорное слово прямо в редакторе.
///
/// `TextEdit` рисует собственное выделение только у виджета в фокусе, а
/// во время прогона редактор только для чтения (`interactive(false)`) и фокус
/// тут же сбрасывается. Поэтому плашку и подчёркивание рисуем сами — ровно то,
/// что делал `Editor.SelStart/SelLength` оригинала.
fn paint_ask_word(
    ui: &egui::Ui,
    out: &egui::widgets::text_edit::TextEditOutput,
    (start, end): (usize, usize),
) {
    let galley = &out.galley;
    if start >= end {
        return;
    }
    let caret_start = galley
        .pos_from_cursor(&galley.from_ccursor(CCursor::new(start)))
        .translate(out.galley_pos.to_vec2());
    let caret_end = galley
        .pos_from_cursor(&galley.from_ccursor(CCursor::new(end)))
        .translate(out.galley_pos.to_vec2());

    let painter = ui.painter_at(out.text_clip_rect);
    let first = caret_start.center().y;
    let last = caret_end.center().y;

    if (first - last).abs() < 0.5 {
        // Слово целиком на одной строке — обычный случай.
        let rect = egui::Rect::from_min_max(
            egui::pos2(caret_start.left(), caret_start.top()),
            egui::pos2(caret_end.left(), caret_start.bottom()),
        );
        paint_word_rect(&painter, rect);
    } else {
        // Слово переносится: подсвечиваем хвост первой строки, все
        // промежуточные целиком и начало последней.
        let mut rect = egui::Rect::from_min_max(
            egui::pos2(caret_start.left(), caret_start.top()),
            egui::pos2(galley.rect.right() + out.galley_pos.x, caret_start.bottom()),
        );
        paint_word_rect(&painter, rect);
        for row in galley.rows.iter() {
            let row_rect = row.rect.translate(out.galley_pos.to_vec2());
            if row_rect.center().y <= first + 0.5 || row_rect.center().y >= last - 0.5 {
                continue;
            }
            rect = row_rect;
            paint_word_rect(&painter, rect);
        }
        let rect = egui::Rect::from_min_max(
            egui::pos2(out.galley_pos.x, caret_end.top()),
            egui::pos2(caret_end.left(), caret_end.bottom()),
        );
        paint_word_rect(&painter, rect);
    }
}

/// Плашка + подчёркивание: заметно, но текст остаётся читаемым.
fn paint_word_rect(painter: &egui::Painter, rect: egui::Rect) {
    painter.rect_filled(rect.expand2(egui::vec2(1.0, 1.0)), 2.0, COLOR_ASK_BG);
    painter.hline(
        rect.x_range(),
        rect.bottom(),
        egui::Stroke::new(1.5_f32, COLOR_MARK),
    );
}

/// Тело панели запроса: рамка с заголовком и ряд из шести кнопок.
///
/// Вынесено из метода, чтобы вёрстку можно было проверить в тестах без окна
/// (см. `mod gui_tests`). Возвращает нажатое решение и прямоугольник ряда —
/// по нему тест убеждается, что кластер стоит ровно по центру.
fn ask_panel_ui(ui: &mut egui::Ui, ask: &AskRequest) -> (Option<AskDecision>, egui::Rect) {
    let title = ask_title(ask);
    let title_color = if ask.wrong_yo || ask.replaced_yo || ask.wrong_ye {
        COLOR_ALERT
    } else {
        COLOR_YE
    };
    let mut decision = None;
    let mut row = egui::Rect::NOTHING;

    ui.add_space(8.0);
    ui.vertical_centered(|ui| {
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new(title).strong().color(title_color));
                    ui.add_space(6.0);
                    // egui не центрирует группу кнопок сам: измеряем ряд и
                    // заказываем ровно такую ширину — тогда кластер встаёт
                    // мёртво по центру, как в оригинале.
                    let buttons = ask_buttons(ask);
                    let size = ask_row_size(ui, &buttons);
                    let response = ui.allocate_ui_with_layout(
                        size,
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            for (text, d, enabled, hint) in buttons {
                                let response = ui
                                    .add_enabled(
                                        enabled,
                                        egui::Button::new(text)
                                            .wrap_mode(egui::TextWrapMode::Extend),
                                    )
                                    .on_hover_text(hint);
                                if response.clicked() {
                                    decision = Some(d);
                                }
                            }
                        },
                    );
                    row = response.response.rect;
                });
            });
    });
    ui.add_space(8.0);

    (decision, row)
}

/// Заголовок панели запроса — точь-в-точь `SetAskParams` из Main.pas.
fn ask_title(ask: &AskRequest) -> &'static str {
    if ask.replaced_yo {
        "Странное расположение «Ё»"
    } else if ask.wrong_yo {
        "Вероятно, здесь должна быть «Е»"
    } else if ask.wrong_ye {
        "Вероятно, здесь должна быть «Ё»"
    } else {
        "Выберите верный вариант"
    }
}

/// Кнопки панели запроса: подпись, решение, доступность и подсказка.
///
/// Набор и подписи повторяют `InitButtons`/`SetButtonCaption` оригинала:
/// `Е (&1)` → `Ё (&2)` → `Всегда Е (&3)` → `Всегда Ё (&4)` → `Откат` →
/// `Прервать (Esc)`, только вместо «Е/Ё» показаны сами варианты слова.
fn ask_buttons(ask: &AskRequest) -> Vec<(egui::RichText, AskDecision, bool, &'static str)> {
    vec![
        (
            egui::RichText::new(format!("{} (1)", ask.word1))
                .monospace()
                .strong()
                .color(COLOR_YE),
            AskDecision::Ye,
            true,
            "Оставить «е»",
        ),
        (
            egui::RichText::new(format!("{} (2)", ask.word2))
                .monospace()
                .strong()
                .color(COLOR_YO),
            AskDecision::Yo,
            true,
            "Поставить «ё»",
        ),
        (
            egui::RichText::new("Всегда Е (3)"),
            AskDecision::YeAll,
            // Подозрительную «ё» оригинал не разрешал запоминать.
            !ask.wrong_yo,
            "Запомнить «е» для этого слова",
        ),
        (
            egui::RichText::new("Всегда Ё (4)"),
            AskDecision::YoAll,
            !ask.wrong_yo,
            "Запомнить «ё» для этого слова",
        ),
        (
            egui::RichText::new("Откат"),
            AskDecision::Undo,
            true,
            "Отменить последнюю замену и остановиться",
        ),
        (
            egui::RichText::new("Прервать (Esc)"),
            AskDecision::Cancel,
            true,
            "Прервать ёфикацию",
        ),
    ]
}

/// Точный размер ряда кнопок: `egui` не центрирует группу сам, поэтому
/// ширину считаем по тем же галетам и отступам, что использует `Button`.
fn ask_row_size(
    ui: &egui::Ui,
    buttons: &[(egui::RichText, AskDecision, bool, &'static str)],
) -> egui::Vec2 {
    let padding = ui.spacing().button_padding;
    let spacing = ui.spacing().item_spacing.x;
    let mut size = egui::vec2(0.0, ui.spacing().interact_size.y);
    for (text, _, _, _) in buttons {
        let galley = egui::WidgetText::from(text.clone()).into_galley(
            ui,
            Some(egui::TextWrapMode::Extend),
            f32::INFINITY,
            egui::TextStyle::Button,
        );
        size.x += galley.size().x + 2.0 * padding.x;
        size.y = size.y.max(galley.size().y + 2.0 * padding.y);
    }
    size.x += spacing * buttons.len().saturating_sub(1) as f32;
    size
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
            "Сохранить текст"
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
                                    format!("[ {name} ]")
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
                ui.checkbox(&mut self.highlight, "Подсвечивать замены в тексте");
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

// ═══════════════════════════════════════════════════════════════════════════
//  Тесты вёрстки
// ═══════════════════════════════════════════════════════════════════════════

/// Проверки интерфейса без окна: `egui::Context` умеет прокручивать кадры
/// вручную, поэтому вёрстку панели запроса и подсветку слова можно
/// проверить обычным `cargo test` — ни дисплея, ни GPU не нужно.
/// Сквозные тесты интерфейса без окна: `YoApp` гоняет настоящий движок в
/// рабочем потоке, а `pump()` переписывает единственный буфер «на месте».
#[cfg(test)]
mod app_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn app() -> YoApp {
        let ctx = egui::Context::default();
        YoApp::with_context(&ctx)
    }

    /// Крутит `pump()` до завершения прогона (или до таймаута).
    fn wait(app: &mut YoApp) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while app.busy() && Instant::now() < deadline {
            app.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!app.busy(), "прогон не завершился за 20 секунд");
    }

    #[test]
    fn run_rewrites_the_single_buffer_in_place() {
        let mut app = app();
        app.text = "Актер вышел на сцену.".to_owned();
        app.text_chars = app.text.chars().count();
        app.start();
        assert!(app.busy(), "прогон не начался");
        wait(&mut app);

        assert_eq!(app.text, "Актёр вышел на сцену.");
        assert_eq!(
            app.text.len(),
            "Актер вышел на сцену.".len(),
            "длина текста должна сохраняться"
        );
        assert_eq!(app.text_chars, "Актер вышел на сцену.".chars().count());
        assert!(
            app.run_source.is_none(),
            "снимок исходника должен освободиться"
        );
        assert!(!app.marks.is_empty(), "замена должна попасть в пометки");
    }

    #[test]
    fn ask_pauses_the_run_and_points_at_the_word() {
        let mut app = app();
        app.opts.always_ask = true;
        app.text = "Рама, актер.".to_owned();
        app.text_chars = app.text.chars().count();
        app.start();

        let deadline = Instant::now() + Duration::from_secs(20);
        while app.ask.is_none() && Instant::now() < deadline {
            app.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        let ask = app.ask.clone().expect("движок должен спросить про «актер»");
        assert_eq!(ask.word1, "актер");
        assert_eq!(ask.word2, "актёр");

        // GUI должен знать, где это слово в единственном буфере.
        let (start, end) = app
            .ask_selection
            .expect("слово должно найтись в буфере по своим смещениям");
        let word: String = app.text.chars().skip(start).take(end - start).collect();
        assert_eq!(word, "актер", "подсветка должна попасть ровно на слово");

        app.answer(AskDecision::Yo);
        assert!(app.ask.is_none(), "ответ должен снять вопрос");
        wait(&mut app);
        assert_eq!(app.text, "Рама, актёр.");
    }

    #[test]
    fn stop_keeps_the_text_consistent() {
        let mut app = app();
        let source = "Актер вышел на сцену, а ёжик остался дома.".repeat(400);
        app.text = source.clone();
        app.text_chars = source.chars().count();
        app.start();
        app.stop();
        wait(&mut app);

        assert_eq!(
            app.text.chars().count(),
            source.chars().count(),
            "после отмены текст не должен ни удлиняться, ни укорачиваться"
        );
        assert!(
            app.status_left.starts_with("прервано"),
            "статус: {}",
            app.status_left
        );
    }
}

/// Тесты единственного буфера: именно здесь текст переписывается «на месте».
#[cfg(test)]
mod buffer_tests {
    use super::*;

    const SOURCE: &str = "Актер вышел на сцену.";

    #[test]
    fn chunk_rewrite_keeps_the_unprocessed_tail() {
        let mut buffer = SOURCE.to_owned();

        // Движок отдал первое слово — остальное возвращается из исходника.
        rewrite_in_place(&mut buffer, Some(SOURCE), 0, "Актёр");
        assert_eq!(buffer, "Актёр вышел на сцену.");
        assert_eq!(
            buffer.len(),
            SOURCE.len(),
            "длина текста не должна меняться"
        );

        // Последняя порция приходит в самый конец — хвоста уже нет.
        rewrite_in_place(&mut buffer, Some(SOURCE), SOURCE.len(), "");
        assert_eq!(buffer, "Актёр вышел на сцену.");
    }

    #[test]
    fn chunk_rewrite_without_a_source_drops_the_tail() {
        // Прогон закончился — хвост исходника больше не нужен.
        let mut buffer = SOURCE.to_owned();
        rewrite_in_place(&mut buffer, None, 0, "Актёр");
        assert_eq!(buffer, "Актёр");
    }

    #[test]
    fn chunk_rewrite_ignores_a_broken_base() {
        let mut buffer = SOURCE.to_owned();
        rewrite_in_place(&mut buffer, Some(SOURCE), 3, "…");
        assert_eq!(
            buffer, SOURCE,
            "база не на границе символа — текст не трогаем"
        );
        rewrite_in_place(&mut buffer, Some(SOURCE), SOURCE.len() + 1, "…");
        assert_eq!(buffer, SOURCE, "база за концом буфера — текст не трогаем");
    }
}

#[cfg(test)]
mod gui_tests {
    use super::*;
    use egui::{pos2, vec2, Context, RawInput, Rect};

    fn ask() -> AskRequest {
        AskRequest {
            word1: "актер".into(),
            word2: "актёр".into(),
            before: "Рама, ".into(),
            after: " вышел".into(),
            wrong_yo: false,
            replaced_yo: false,
            wrong_ye: false,
            focus_first: true,
            word_start: 10,
            word_end: 20,
        }
    }

    fn raw(screen: egui::Vec2, time: f64) -> RawInput {
        RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), screen)),
            time: Some(time),
            ..Default::default()
        }
    }

    fn click_events(pos: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    /// Прогоняет панель запроса дважды (первый кадр — прогрев шрифтов) и
    /// возвращает решение вместе с прямоугольником ряда кнопок.
    fn run_panel(ask: &AskRequest, screen: egui::Vec2) -> (Option<AskDecision>, Rect) {
        let ctx = Context::default();
        let mut result = (None, Rect::NOTHING);
        for frame in 0..2 {
            let mut inner = (None, Rect::NOTHING);
            let _ = ctx.run(raw(screen, frame as f64 * 0.016), |ctx| {
                egui::TopBottomPanel::bottom("ask_panel").show(ctx, |ui| {
                    inner = ask_panel_ui(ui, ask);
                });
            });
            result = inner;
        }
        result
    }

    #[test]
    fn ask_row_is_dead_center() {
        let screen = vec2(1000.0, 700.0);
        let (_, row) = run_panel(&ask(), screen);
        assert!(row.width() > 0.0, "ряд кнопок не отрисован");
        assert!(
            (row.center().x - screen.x / 2.0).abs() < 0.5,
            "ряд должен стоять по центру: {:?}, центр экрана {}",
            row,
            screen.x / 2.0
        );
        assert!(
            row.width() <= screen.x,
            "ряд шире окна: {} > {}",
            row.width(),
            screen.x
        );
    }

    #[test]
    fn ask_row_stays_inside_a_narrow_window() {
        // Минимальный размер окна из `NativeOptions`.
        let screen = vec2(720.0, 480.0);
        let (_, row) = run_panel(&ask(), screen);
        assert!(row.width() > 0.0, "ряд кнопок не отрисован");
        assert!(
            row.min.x >= 0.0 && row.max.x <= screen.x,
            "ряд вылез: {row:?}"
        );
    }

    #[test]
    fn ask_buttons_follow_the_original_order() {
        let buttons = ask_buttons(&ask());
        let captions: Vec<String> = buttons
            .iter()
            .map(|(t, _, _, _)| t.text().to_owned())
            .collect();
        assert_eq!(
            captions,
            [
                "актер (1)",
                "актёр (2)",
                "Всегда Е (3)",
                "Всегда Ё (4)",
                "Откат",
                "Прервать (Esc)",
            ]
        );
        let decisions: Vec<AskDecision> = buttons.iter().map(|(_, d, _, _)| *d).collect();
        assert!(matches!(
            decisions.as_slice(),
            [
                AskDecision::Ye,
                AskDecision::Yo,
                AskDecision::YeAll,
                AskDecision::YoAll,
                AskDecision::Undo,
                AskDecision::Cancel
            ]
        ));
    }

    #[test]
    fn suspicious_yo_disables_the_remember_buttons() {
        // `bYeAll.Enabled := not WrongYo` из Main.pas.
        let mut a = ask();
        a.wrong_yo = true;
        let enabled: Vec<bool> = ask_buttons(&a).iter().map(|(_, _, e, _)| *e).collect();
        assert_eq!(enabled, [true, true, false, false, true, true]);
        assert_eq!(ask_title(&a), "Вероятно, здесь должна быть «Е»");
    }

    #[test]
    fn ask_titles_match_the_original() {
        let mut a = ask();
        assert_eq!(ask_title(&a), "Выберите верный вариант");
        a.wrong_ye = true;
        assert_eq!(ask_title(&a), "Вероятно, здесь должна быть «Ё»");
        a.wrong_ye = false;
        a.replaced_yo = true;
        assert_eq!(ask_title(&a), "Странное расположение «Ё»");
    }

    #[test]
    fn clicking_a_button_reports_its_decision() {
        let ctx = Context::default();
        let screen = vec2(1000.0, 700.0);
        let a = ask();

        // Пара кадров раскладки: высота нижней панели запоминается с
        // предыдущего кадра, поэтому позиции кнопок стабильны только со
        // второго кадра.
        let mut row = Rect::NOTHING;
        for frame in 0..2 {
            let _ = ctx.run(raw(screen, frame as f64 * 0.016), |ctx| {
                egui::TopBottomPanel::bottom("ask_panel").show(ctx, |ui| {
                    row = ask_panel_ui(ui, &a).1;
                });
            });
        }

        // Кадры 2 и 3 — нажатие и отпускание на первой кнопке («Е»).
        let pos = row.left_center() + vec2(row.height() * 0.4, 0.0);
        let mut clicked = None;
        for (frame, pressed) in [true, false].into_iter().enumerate() {
            let mut input = raw(screen, 0.016 * (frame as f64 + 2.0));
            input.events = click_events(pos, pressed);
            let _ = ctx.run(input, |ctx| {
                egui::TopBottomPanel::bottom("ask_panel").show(ctx, |ui| {
                    let (d, _) = ask_panel_ui(ui, &a);
                    if d.is_some() {
                        clicked = d;
                    }
                });
            });
        }

        assert!(
            matches!(clicked, Some(AskDecision::Ye)),
            "клик по первой кнопке должен вернуть Ye, получено {clicked:?}"
        );
    }

    /// Подсветка слова в редакторе: `paint_ask_word` рисует плашку ровно под
    /// словом, даже когда редактор только для чтения и без фокуса.
    #[test]
    fn ask_word_is_highlighted_in_the_editor() {
        let ctx = Context::default();
        let screen = vec2(800.0, 600.0);
        let a = ask();
        let mut text = String::from("Рама, актер вышел на сцену.");
        let id = text_edit_id();
        let range = a.char_range(&text).expect("слово должно попадать в текст");

        let mut carets = None;
        for frame in 0..2 {
            let _ = ctx.run(raw(screen, frame as f64 * 0.016), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let out = egui::TextEdit::multiline(&mut text)
                        .id(id)
                        .interactive(false)
                        .show(ui);
                    paint_ask_word(ui, &out, range);
                    let s = out
                        .galley
                        .pos_from_cursor(&out.galley.from_ccursor(CCursor::new(range.0)))
                        .translate(out.galley_pos.to_vec2());
                    let e = out
                        .galley
                        .pos_from_cursor(&out.galley.from_ccursor(CCursor::new(range.1)))
                        .translate(out.galley_pos.to_vec2());
                    carets = Some((s, e));
                });
            });
        }
        let (s, e) = carets.expect("кадр не отрисован");

        // Ищем плашку подсветки среди фигур последнего кадра.
        let out = ctx.run(raw(screen, 0.048), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let out = egui::TextEdit::multiline(&mut text)
                    .id(id)
                    .interactive(false)
                    .show(ui);
                paint_ask_word(ui, &out, range);
            });
        });
        let plaques: Vec<Rect> = out
            .shapes
            .iter()
            .filter_map(|cs| match &cs.shape {
                egui::epaint::Shape::Rect(r) if r.fill == COLOR_ASK_BG => Some(r.rect),
                _ => None,
            })
            .collect();
        assert!(!plaques.is_empty(), "плашка подсветки не нарисована");

        let expected = Rect::from_min_max(
            pos2(s.left() - 2.0, s.top() - 2.0),
            pos2(e.left() + 2.0, s.bottom() + 2.0),
        );
        assert!(
            plaques.iter().any(|r| r.intersects(expected)
                && (r.center().x - expected.center().x).abs() < expected.width() / 2.0),
            "плашка {plaques:?} не совпала со словом {expected:?}"
        );
    }
}
