#![windows_subsystem = "windows"]
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
//!
//! ## Настройки
//!
//! Всё, что можно переключить, живёт в меню «Настройки» (в главном окне нет
//! ни одного флажка — как в Delphi-оригинале, где опции прятались в диалоге).
//! Значения хранятся в `yo.ini` рядом с исполняемым файлом и пишутся при
//! выходе; см. [`yol::settings`].
//!
//! ## Горячие клавиши
//!
//! | Клавиши | Действие |
//! |---------|----------|
//! | `Ctrl+L` | загрузить текст |
//! | `Ctrl+S` | сохранить как |
//! | `Ctrl+Alt+S` | сохранить |
//! | `Ctrl+P` | свойства файла |
//! | `F9` / `Ctrl+F9` | ёфицировать с текущей позиции / с начала |
//! | `Shift+F10` | убрать подсветку замен |
//! | `Ctrl+F`, `Ctrl+R`, `F3` | найти, найти по регулярному выражению, найти снова |
//! | `Ctrl+W`, `Ctrl+Q` | искать замену вперёд / назад |
//! | `Esc` | прервать ёфикацию |

mod fonts;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use egui::text::{CCursor, CCursorRange};
use egui::widgets::text_edit::TextEditState;
use egui::FontFamily;

// Ядро живёт в библиотеке `yol` (src/lib.rs → src/yo_core.rs), а не подключается
// модулем: так его же можно покрывать интеграционными тестами из tests/.
use yol::settings::{
    default_ini_path, FontSpec, FontStyle, GuiSettings, Rgb, Rgba, MAX_FONT_SIZE, MAX_RECENT,
    MIN_FONT_SIZE,
};
use yol::yo_core::{
    copy_case, database_info, decode_bytes, decode_cp1251, encode_string, preload, utf16_units,
    AmbiguousPolicy, AskDecision, AskRequest, Dictionary, Engine, RegexDict, Sink, Stats,
    TextEncoding, YoOptions,
};

use fonts::FontCatalog;

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
const COLOR_YE: egui::Color32 = egui::Color32::from_rgb(120, 190, 255);
const COLOR_YO: egui::Color32 = egui::Color32::from_rgb(150, 240, 150);
const COLOR_DIM: egui::Color32 = egui::Color32::from_rgb(150, 150, 160);
/// «Тревожный» заголовок панели запроса (в оригинале — `clRed`).
const COLOR_ALERT: egui::Color32 = egui::Color32::from_rgb(255, 120, 120);

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

/// Что выбирает диалог файла: текст, словарь, регулярные выражения или шрифт.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FilePurpose {
    Text,
    Dictionary,
    RegexDictionary,
    Font,
}

impl FilePurpose {
    /// Расширения, которые имеет смысл показывать.
    fn extensions(self) -> &'static [&'static str] {
        match self {
            FilePurpose::Text => &["txt", "fb2", "rtf", "md", "log", "ini", "csv"],
            FilePurpose::Dictionary => &["dat", "txt"],
            FilePurpose::RegexDictionary => &["dat", "txt"],
            FilePurpose::Font => &["ttf", "otf"],
        }
    }

    fn title(self) -> &'static str {
        match self {
            FilePurpose::Text => "Загрузить текст",
            FilePurpose::Dictionary => "Загрузить основной словарь",
            FilePurpose::RegexDictionary => "Загрузить словарь регулярных выражений",
            FilePurpose::Font => "Загрузить шрифт",
        }
    }
}

enum FileAction {
    Load(PathBuf, TextEncoding),
    Save(PathBuf, TextEncoding),
    OpenDictionary(PathBuf),
    OpenRegexDictionary(PathBuf),
    OpenFont(PathBuf),
}

struct FileDialog {
    open: bool,
    is_save: bool,
    purpose: FilePurpose,
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
            purpose: FilePurpose::Text,
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

    /// Открыть диалог на чтение файла нужного назначения.
    fn open_for(&mut self, purpose: FilePurpose, dir: Option<&Path>) {
        self.open = true;
        self.is_save = false;
        self.purpose = purpose;
        self.status.clear();
        if let Some(dir) = dir.filter(|d| d.is_dir()) {
            self.dir = dir.to_path_buf();
        }
        self.refresh();
    }

    fn open_for_save(&mut self, suggested: &str) {
        self.open = true;
        self.is_save = true;
        self.purpose = FilePurpose::Text;
        self.status.clear();
        self.name = suggested.to_owned();
        self.refresh();
    }

    fn title(&self) -> &'static str {
        if self.is_save {
            "Сохранить текст"
        } else {
            self.purpose.title()
        }
    }
}

/// Содержимое каталога: сначала папки, потом файлы, всё по алфавиту.
///
/// `Some(exts)` оставляет только файлы с перечисленными расширениями
/// (папки показываются всегда — иначе из каталога не выбраться).
fn list_dir_filtered(dir: &Path, exts: Option<&[&str]>) -> Vec<(String, bool)> {
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
        } else if let Some(exts) = exts {
            let ext = Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase());
            if ext.as_deref().is_some_and(|e| exts.contains(&e)) {
                files.push(name);
            }
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

/// Содержимое каталога без фильтра по расширениям.
fn list_dir(dir: &Path) -> Vec<(String, bool)> {
    list_dir_filtered(dir, None)
}

/// Корни файловой системы: диски на Windows, точки монтирования на Unix.
///
/// Без этого список файлов умел ходить только вверх по текущему дереву —
/// переключиться с `C:` на `D:` из диалога было нельзя.
fn system_roots() -> Vec<(String, PathBuf)> {
    let mut roots: Vec<(String, PathBuf)> = Vec::new();
    if cfg!(windows) {
        for c in b'A'..=b'Z' {
            let path = PathBuf::from(format!("{}:\\", c as char));
            if path.is_dir() {
                roots.push((format!("{}:", c as char), path));
            }
        }
    } else {
        roots.push(("/".to_owned(), PathBuf::from("/")));
        for dir in ["/mnt", "/media", "/home", "/usr/local", "/opt", "/tmp"] {
            let path = PathBuf::from(dir);
            if path.is_dir() {
                roots.push((dir.to_owned(), path));
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            let path = PathBuf::from(home);
            if path.is_dir() {
                let label = format!("~ ({})", path.display());
                roots.push((label, path));
            }
        }
    }
    roots
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
//  Шрифт, поиск и свойства файла
// ═══════════════════════════════════════════════════════════════════════════

/// Имя, под которым внешний шрифт регистрируется в egui.
const EDITOR_FONT_KEY: &str = "yol_editor_face";

/// Разрешённый шрифт редактора.
#[derive(Clone, Debug)]
struct EditorFont {
    family: FontFamily,
    size: f32,
    /// Курсив придётся синтезировать (у файла нет курсивного начертания).
    italics: bool,
}

impl EditorFont {
    fn font_id(&self) -> egui::FontId {
        egui::FontId::new(self.size, self.family.clone())
    }
}

/// Встроенное семейство по имени из настроек.
fn builtin_family(name: &str) -> FontFamily {
    match name {
        "Hack" | "Monospace" | "Моноширинный" => FontFamily::Monospace,
        _ => FontFamily::Proportional,
    }
}

/// Собрать [`egui::FontDefinitions`]: встроенные семейства плюс (если задан)
/// внешний файл шрифта.
fn build_font_definitions(spec: &FontSpec) -> (egui::FontDefinitions, ResolvedFont) {
    let mut fonts = egui::FontDefinitions::default();
    let mut family = builtin_family(&spec.family);
    let mut italics = spec.style.is_italic();
    let mut loaded = None;

    if let Some(file) = &spec.file {
        if let Ok(bytes) = std::fs::read(file) {
            if let Some(names) = fonts::read_face_names(&bytes) {
                // Курсив уже есть в самом файле — синтезировать не нужно.
                italics = spec.style.is_italic() && !names.style.is_italic();
            }
            fonts.font_data.insert(
                EDITOR_FONT_KEY.to_owned(),
                Arc::new(egui::FontData::from_owned(bytes)),
            );
            // Основной шрифт плюс Ubuntu-Light как запасной для недостающих
            // глифов (стрелки, знак номера и т.п.).
            fonts.families.insert(
                FontFamily::Name(EDITOR_FONT_KEY.into()),
                vec![EDITOR_FONT_KEY.to_owned(), "Ubuntu-Light".to_owned()],
            );
            family = FontFamily::Name(EDITOR_FONT_KEY.into());
            loaded = Some(file.clone());
        }
    }

    (
        fonts,
        ResolvedFont {
            family,
            italics,
            loaded,
        },
    )
}

/// Вспомогательная структура: что получилось из [`build_font_definitions`].
struct ResolvedFont {
    family: FontFamily,
    italics: bool,
    loaded: Option<PathBuf>,
}

/// Применить шрифты к контексту и вернуть разрешённый [`EditorFont`].
fn install_fonts(ctx: &egui::Context, spec: &FontSpec) -> EditorFont {
    let (fonts, resolved) = build_font_definitions(spec);
    ctx.set_fonts(fonts);
    let _ = resolved.loaded;
    EditorFont {
        family: resolved.family,
        size: spec.size,
        italics: resolved.italics,
    }
}

/// Загрузить внешний словарь. Живёт до конца процесса (их единицы за сеанс).
fn load_dictionary(path: &Path) -> Option<&'static Dictionary> {
    let bytes = std::fs::read(path).ok()?;
    let dict = Dictionary::parse(&bytes);
    if dict.is_empty() {
        return None;
    }
    Some(Box::leak(Box::new(dict)))
}

/// Загрузить внешний словарь регулярных выражений.
fn load_regex_dictionary(path: &Path) -> Option<&'static RegexDict> {
    let bytes = std::fs::read(path).ok()?;
    let dict = RegexDict::parse(&bytes);
    if dict.is_empty() {
        return None;
    }
    Some(Box::leak(Box::new(dict)))
}

/// Что выбрал пользователь в контекстном меню редактора.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditorMenu {
    Copy,
    Paste,
    Clear,
}

/// Состояние поиска по тексту.
#[derive(Default, Clone, Debug)]
struct FindState {
    query: String,
    /// Искать как регулярное выражение.
    regex: bool,
    case_sensitive: bool,
    error: Option<String>,
    /// Последнее найденное совпадение — в индексах символов.
    last: Option<(usize, usize)>,
}

/// Сведения о загруженном файле («Файл → Свойства файла»).
#[derive(Clone, Debug)]
struct FileProps {
    path: Option<PathBuf>,
    exists: bool,
    size: u64,
    modified: Option<SystemTime>,
    encoding: TextEncoding,
    chars: usize,
    words: usize,
    lines: usize,
}

impl FileProps {
    fn collect(path: Option<&Path>, text: &str, encoding: TextEncoding) -> Self {
        let meta = path.and_then(|p| std::fs::metadata(p).ok());
        Self {
            path: path.map(Path::to_path_buf),
            exists: meta.is_some(),
            size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
            modified: meta.and_then(|m| m.modified().ok()),
            encoding,
            chars: text.chars().count(),
            words: text
                .split(|c: char| !yol::yo_core::is_alpha(c))
                .filter(|w| !w.is_empty())
                .count(),
            lines: if text.is_empty() {
                0
            } else {
                text.lines().count().max(1)
            },
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

    // ── настройки и оформление ──────────────────────────────────────────
    /// Откуда читать и куда писать `yo.ini`; `None` — не сохранять вовсе
    /// (так живут тесты и встраиваемые сборки).
    config_path: Option<PathBuf>,
    /// Редактор: семейство, файл, начертание, размер.
    font_spec: FontSpec,
    /// Разрешённый `FontId` и признак синтетического курсива.
    editor_font: EditorFont,
    /// Найденные системные шрифты (скан ленивый — при открытии диалога).
    font_catalog: FontCatalog,
    font_catalog_scanned: bool,

    mark_text: Rgb,
    mark_bg: Rgb,
    ask_text: Rgb,
    ask_plate: Rgba,
    ask_underline: f32,

    /// Последние открытые файлы (свежие первыми).
    recent: Vec<PathBuf>,

    // ── поиск ───────────────────────────────────────────────────────────
    show_find: bool,
    find: FindState,
    /// К какому символу прокрутить редактор на ближайшем кадре.
    scroll_to_selection: Option<usize>,

    // ── окна ────────────────────────────────────────────────────────────
    show_font_dialog: bool,
    show_colors: bool,
    show_props: bool,
    props: Option<FileProps>,

    // ── словари ─────────────────────────────────────────────────────────
    /// Внешний основной словарь (если загружен пользователем).
    dict_override: Option<&'static Dictionary>,
    /// Внешний словарь регулярных выражений.
    re_override: Option<&'static RegexDict>,
    /// Откуда они взяты — эти пути уходят в `yo.ini`.
    dictionary_path: Option<PathBuf>,
    regex_dict_path: Option<PathBuf>,
}

impl YoApp {
    /// Настоящий запуск: настройки читаются из `yo.ini` рядом с бинарником.
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let path = default_ini_path();
        let settings = GuiSettings::load(&path);
        Self::with_settings(&cc.egui_ctx, settings, Some(path))
    }

    /// Сборка приложения с готовым `egui::Context` и настройками по умолчанию:
    /// так `YoApp` поднимают тесты, где ни окна, ни `yo.ini` нет.
    #[cfg_attr(not(test), allow(dead_code))]
    fn with_context(ctx: &egui::Context) -> Self {
        Self::with_settings(ctx, GuiSettings::default(), None)
    }

    /// Общий конструктор: применяет тему и шрифты, подхватывает внешние
    /// словари и (если включено) последний открытый файл.
    fn with_settings(
        ctx: &egui::Context,
        settings: GuiSettings,
        config_path: Option<PathBuf>,
    ) -> Self {
        // Словари (1.8 МБ) разбираем в фоне: окно появляется мгновенно,
        // а первый прогон всё равно дождётся готовности базы.
        std::thread::spawn(preload);

        apply_theme(ctx, settings.dark);
        let editor_font = install_fonts(ctx, &settings.font);

        // Внешние базы, выбранные в прошлый раз, читаем сразу: без них
        // первый прогон дал бы не те замены.
        let dict_override = settings.dictionary.as_deref().and_then(load_dictionary);
        let re_override = settings
            .regex_dictionary
            .as_deref()
            .and_then(load_regex_dictionary);

        let mut app = Self {
            opts: settings.opts.clone(),
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
            dark: settings.dark,
            applied_dark: Some(settings.dark),
            highlight: settings.highlight,
            show_journal: settings.show_journal,
            show_options: false,
            show_about: false,
            show_stats: false,
            dialog: FileDialog::new(),
            current_file: None,
            current_encoding: TextEncoding::Utf8Bom,
            config_path,
            font_spec: settings.font.clone(),
            editor_font,
            font_catalog: FontCatalog::default(),
            font_catalog_scanned: false,
            mark_text: settings.mark_text,
            mark_bg: settings.mark_bg,
            ask_text: settings.ask_text,
            ask_plate: settings.ask_plate,
            ask_underline: settings.ask_underline,
            recent: settings.recent.clone(),
            show_find: false,
            find: FindState::default(),
            scroll_to_selection: None,
            show_font_dialog: false,
            show_colors: false,
            show_props: false,
            props: None,
            dict_override,
            re_override,
            dictionary_path: settings.dictionary.clone(),
            regex_dict_path: settings.regex_dictionary.clone(),
        };

        // «Помнить последний файл» — как `RecallLastFile` оригинала.
        if app.opts.last_file {
            if let Some(path) = settings.last_file.clone().filter(|p| p.is_file()) {
                app.load_path(path, None);
            }
        }
        app
    }

    /// Снимок настроек для записи в `yo.ini`.
    fn settings_snapshot(&self) -> GuiSettings {
        GuiSettings {
            opts: self.opts.clone(),
            dark: self.dark,
            highlight: self.highlight,
            show_journal: self.show_journal,
            font: self.font_spec.clone(),
            mark_text: self.mark_text,
            mark_bg: self.mark_bg,
            ask_text: self.ask_text,
            ask_plate: self.ask_plate,
            ask_underline: self.ask_underline,
            recent: self.recent.clone(),
            // Если файл ещё не открывали, не теряем прошлый «последний».
            last_file: self
                .current_file
                .clone()
                .or_else(|| self.recent.first().cloned()),
            dictionary: self.dictionary_path.clone(),
            regex_dictionary: self.regex_dict_path.clone(),
        }
    }

    /// Записать `yo.ini`. Ошибка не фатальна — показываем её в строке
    /// состояния, чтобы пользователь знал, что настройки не сохранились.
    fn save_settings(&mut self) {
        let Some(path) = self.config_path.clone() else {
            return;
        };
        let settings = self.settings_snapshot();
        if let Err(e) = settings.save(&path) {
            self.status_left = format!("не удалось сохранить {}: {e}", path.display());
        }
    }

    #[inline]
    fn busy(&self) -> bool {
        self.run.is_some()
    }

    // ────────────────────────────────────────────────────────────────────
    //  Запуск / остановка / обмен с рабочим потоком
    // ────────────────────────────────────────────────────────────────────

    /// Ёфицировать с начала текста (`Ctrl+F9`).
    fn start(&mut self) {
        self.start_at(0);
    }

    /// Ёфицировать с байтового смещения `from` (`F9` — с текущей позиции).
    fn start_at(&mut self, from: usize) {
        if self.busy() {
            return;
        }
        if self.text.is_empty() {
            self.status_left = "нечего ёфицировать".to_owned();
            return;
        }
        // Движок не умеет начинать посреди символа.
        let mut from = from.min(self.text.len());
        while from > 0 && !self.text.is_char_boundary(from) {
            from -= 1;
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
        // Активные базы: встроенные или загруженные пользователем.
        let dict = self.dict_override.unwrap_or_else(yol::yo_core::dictionary);
        let re = self
            .re_override
            .unwrap_or_else(yol::yo_core::regex_dictionary);

        std::thread::spawn(move || {
            let (words, dic_version, rules, re_version) = (
                dict.len(),
                dict.version().to_owned(),
                re.len(),
                re.version().to_owned(),
            );
            let _ = tx.send(Event::Ready {
                words,
                dic_version,
                rules,
                re_version,
                elapsed_ms: 0,
            });

            let mut engine = Engine::with_databases(opts, dict, re);
            engine.set_journal(want_journal);
            let mut sink = GuiSink {
                tx: &tx,
                rx: &ans_rx,
                cancel: &cancel_thread,
            };
            let stats = engine.process_from(engine_input.as_str(), from, &mut sink);
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
        self.status_left = if from == 0 {
            "ёфикация с начала текста…".to_owned()
        } else {
            format!("ёфикация с позиции {from}…")
        };
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
        // Вопрос мог быть поднят и без прогона («Искать замену»): тогда
        // отвечать движку некому, но журнал и состояние обновить нужно.
        let tx = self.run.as_ref().map(|r| r.ans_tx.clone());
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
        if let Some(tx) = tx {
            let _ = tx.send(decision);
        }
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
                self.remember_file(&path);
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
                self.remember_file(&path);
                self.status_left = format!(
                    "сохранено: {} · {}",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    enc.label()
                );
            }
            Err(e) => self.status_left = format!("ошибка записи: {e}"),
        }
    }

    // ────────────────────────────────────────────────────────────────────
    //  Файлы, буфер и свойства
    // ────────────────────────────────────────────────────────────────────

    /// Открыть диалог загрузки текста в папке текущего файла.
    fn open_load_dialog(&mut self) {
        let dir = self
            .current_file
            .as_deref()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf);
        self.dialog.open_for(FilePurpose::Text, dir.as_deref());
        if let Some(name) = self
            .current_file
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
        {
            self.dialog.name = name.to_owned();
        }
        self.dialog.encoding = self.current_encoding;
    }

    /// «Сохранить» (`ask == false`) или «Сохранить как» (`ask == true`).
    fn save_text(&mut self, ask: bool) {
        if self.busy() {
            self.status_left = "дождитесь окончания ёфикации".to_owned();
            return;
        }
        if !ask {
            if let Some(path) = self.current_file.clone() {
                self.save_path(path, self.current_encoding);
                return;
            }
        }
        let suggested = self
            .current_file
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("текст.txt")
            .to_owned();
        self.dialog.open_for_save(&suggested);
        if let Some(dir) = self.current_file.as_deref().and_then(|p| p.parent()) {
            self.dialog.dir = dir.to_path_buf();
            self.dialog.refresh();
        }
        self.dialog.encoding = self.current_encoding;
    }

    /// «Очистить текстовое окно».
    fn clear_text(&mut self) {
        if self.busy() {
            self.status_left = "дождитесь окончания ёфикации".to_owned();
            return;
        }
        self.text.clear();
        self.text_chars = 0;
        self.run_source = None;
        self.marks.clear();
        self.journal.clear();
        self.stats = Stats::default();
        self.ask = None;
        self.ask_selection = None;
        self.selection_after = None;
        self.current_file = None;
        self.status_left = "окно очищено".to_owned();
    }

    /// Запомнить файл в списке последних (без дублей).
    fn remember_file(&mut self, path: &Path) {
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_path_buf());
        self.recent.truncate(MAX_RECENT);
    }

    /// «Файл → Свойства файла» (`Ctrl+P`).
    fn open_props(&mut self) {
        self.props = Some(FileProps::collect(
            self.current_file.as_deref(),
            &self.text,
            self.current_encoding,
        ));
        self.show_props = true;
    }

    // ────────────────────────────────────────────────────────────────────
    //  Ёфикация
    // ────────────────────────────────────────────────────────────────────

    /// `F9`: ёфицировать с начала слова под курсором.
    fn yoficate_from_cursor(&mut self, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        let pos = self.cursor_char_index(ctx);
        let byte = char_to_byte(&self.text, pos);
        self.start_at(byte);
    }

    /// Позиция курсора в символах (0, если редактор ещё не трогали).
    fn cursor_char_index(&self, ctx: &egui::Context) -> usize {
        TextEditState::load(ctx, text_edit_id())
            .and_then(|s| s.cursor.char_range())
            .map(|r| r.primary.index.min(self.text_chars))
            .unwrap_or(0)
    }

    /// Убрать подсветку замен (`Shift+F10`).
    fn remove_marks(&mut self) {
        self.marks.clear();
        self.status_left = "подсветка замен убрана".to_owned();
    }

    /// «Сбросить „Всегда Е/Ё“» — забыть ответы, запомненные пользователем.
    fn reset_user_selections(&mut self) {
        if self.busy() {
            self.status_left = "дождитесь окончания ёфикации".to_owned();
            return;
        }
        if let Some(dict) = self.dict_override {
            dict.clear_user_sel();
        } else {
            yol::yo_core::dictionary().clear_user_sel();
        }
        self.status_left = "запомненные ответы сброшены".to_owned();
    }

    // ────────────────────────────────────────────────────────────────────
    //  Поиск
    // ────────────────────────────────────────────────────────────────────

    fn open_find(&mut self, regex: bool) {
        self.show_find = true;
        self.find.regex = regex;
        self.find.error = None;
    }

    /// Текущая позиция курсора — с неё начинается следующий поиск.
    fn search_start(&self, ctx: &egui::Context) -> usize {
        match TextEditState::load(ctx, text_edit_id()).and_then(|s| s.cursor.char_range()) {
            Some(r) => r.primary.index.max(r.secondary.index),
            None => 0,
        }
    }

    /// Найти следующее совпадение и выделить его (с переносом через край).
    fn find_again(&mut self, ctx: &egui::Context, forward: bool) {
        if self.find.query.is_empty() {
            self.open_find(false);
            return;
        }
        let from = self.search_start(ctx);
        let found = if self.find.regex {
            match regex_find(
                &self.text,
                &self.find.query,
                from,
                forward,
                self.find.case_sensitive,
            ) {
                Ok(v) => {
                    self.find.error = None;
                    v
                }
                Err(e) => {
                    self.find.error = Some(e);
                    None
                }
            }
        } else {
            text_find(
                &self.text,
                &self.find.query,
                from,
                forward,
                self.find.case_sensitive,
            )
        };
        match found {
            Some((s, e)) => {
                self.find.last = Some((s, e));
                self.select_chars(ctx, s, e);
                self.scroll_to_selection = Some(s);
                let snippet: String = self
                    .text
                    .chars()
                    .skip(s)
                    .take(e.saturating_sub(s).min(40))
                    .collect();
                self.status_left = format!("найдено: {snippet}");
            }
            None => {
                self.status_left = format!("не найдено: {}", self.find.query);
                self.find.last = None;
            }
        }
    }

    /// Выделить диапазон в **символах** и дать редактору фокус.
    fn select_chars(&mut self, ctx: &egui::Context, start: usize, end: usize) {
        let id = text_edit_id();
        let mut state = TextEditState::load(ctx, id).unwrap_or_default();
        state.cursor.set_char_range(Some(CCursorRange::two(
            CCursor::new(start),
            CCursor::new(end),
        )));
        state.store(ctx, id);
        ctx.memory_mut(|m| m.request_focus(id));
    }

    /// «Искать замену вперёд/назад» (`Ctrl+W` / `Ctrl+Q`).
    ///
    /// Ищет в тексте ближайшее слово, для которого словарь знает замену,
    /// отличную от написанного, и выделяет его — так же, как это делал
    /// `FindNextYofication` оригинала. Поиск зациклен.
    fn find_yofication_word(&mut self, ctx: &egui::Context, forward: bool) {
        if self.text.is_empty() {
            self.status_left = "нечего искать — текст пуст".to_owned();
            return;
        }
        let dict = self.dict_override.unwrap_or_else(yol::yo_core::dictionary);
        let chars: Vec<char> = self.text.chars().collect();
        let n = chars.len();
        let from = self.search_start(ctx).min(n);

        // Обходим слова в нужную сторону с переносом через край.
        let order: Vec<usize> = if forward {
            (from..n).chain(0..from).collect()
        } else {
            (0..from).rev().chain((from..n).rev()).collect()
        };
        let mut i = 0usize;
        while i < order.len() {
            let at = order[i];
            if !yol::yo_core::is_alpha(chars[at]) {
                i += 1;
                continue;
            }
            let mut s = at;
            while s > 0 && yol::yo_core::is_alpha(chars[s - 1]) {
                s -= 1;
            }
            let mut e = at;
            while e < n && yol::yo_core::is_alpha(chars[e]) {
                e += 1;
            }
            let word: String = chars[s..e].iter().collect();
            if let Some((ye, yo)) = dictionary_variants(dict, &word) {
                let current = &word;
                let differs = current != &ye && current != &yo;
                if differs && (ye != yo || !current.eq_ignore_ascii_case(&ye)) {
                    self.select_chars(ctx, s, e);
                    self.scroll_to_selection = Some(s);
                    self.status_left = format!("замена: {ye} → {yo}");
                    return;
                }
            }
            // Перепрыгиваем слово целиком, чтобы не проверять его по буквам.
            i += (e - at).max(1);
        }
        self.status_left = format!(
            "дальше {} замен не найдено",
            if forward { "" } else { "назад " }
        );
    }

    // ────────────────────────────────────────────────────────────────────
    //  Горячие клавиши
    // ────────────────────────────────────────────────────────────────────

    /// Обработать глобальные сочетания клавиш.
    ///
    /// Список повторяет меню Delphi-оригинала; `Ctrl+X/C/V/A/Z` не трогаем —
    /// их обслуживает сам `TextEdit` нативными событиями egui.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, Modifiers};

        let ctrl = Modifiers::CTRL;
        let ctrl_alt = Modifiers::CTRL.plus(Modifiers::ALT);
        let shift = Modifiers::SHIFT;

        if shortcut(ctx, ctrl, Key::L) {
            self.open_load_dialog();
        }
        if shortcut(ctx, ctrl, Key::S) {
            self.save_text(true);
        }
        if shortcut(ctx, ctrl_alt, Key::S) {
            self.save_text(false);
        }
        if shortcut(ctx, ctrl, Key::P) {
            self.open_props();
        }
        // Как в Delphi-оригинале: Ctrl+O и Ctrl+Alt+O открывают «Опции».
        if shortcut(ctx, ctrl_alt, Key::O) || shortcut(ctx, ctrl, Key::O) {
            self.show_options = true;
        }
        if shortcut(ctx, ctrl, Key::F) {
            self.open_find(false);
        }
        if shortcut(ctx, ctrl, Key::R) {
            self.open_find(true);
        }
        if shortcut(ctx, Modifiers::NONE, Key::F3) {
            self.find_again(ctx, true);
        }
        if shortcut(ctx, ctrl, Key::W) {
            self.find_yofication_word(ctx, true);
        }
        if shortcut(ctx, ctrl, Key::Q) {
            self.find_yofication_word(ctx, false);
        }
        if shortcut(ctx, ctrl, Key::F9) {
            self.start();
        } else if shortcut(ctx, Modifiers::NONE, Key::F9) {
            self.yoficate_from_cursor(ctx);
        }
        if shortcut(ctx, shift, Key::F10) {
            self.remove_marks();
        }
        if shortcut(ctx, Modifiers::NONE, Key::F1) {
            self.show_about = true;
        }
        // Esc: сначала закрывает запрос (это делает панель), затем прерывает
        // прогон — как «Прервать (Esc)» оригинала.
        if self.ask.is_none() && self.busy() && ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.stop();
        }
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
        let ask_text = color32(self.ask_text);
        let plate = color32a(self.ask_plate);
        egui::TopBottomPanel::bottom("ask_panel")
            .resizable(false)
            // Высота запоминается с предыдущего кадра; запас на первый кадр,
            // чтобы кнопки не оказались за нижним краем окна.
            .default_height(88.0)
            .show(ctx, |ui| {
                let (clicked, _) = ask_panel_ui(ui, &ask, ask_text, plate);
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

    // ── главное меню ───────────────────────────────────────────────────
    fn menu_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                self.file_menu(ui, ctx);
                self.edit_menu(ui, ctx);
                self.yofication_menu(ui, ctx);
                self.settings_menu(ui);
                self.help_menu(ui);

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

    /// Пункт меню с подписью и «горячей клавишей» справа.
    fn menu_item(ui: &mut egui::Ui, label: &str, shortcut: &str, enabled: bool) -> bool {
        let button = if shortcut.is_empty() {
            egui::Button::new(label)
        } else {
            egui::Button::new(label).shortcut_text(shortcut)
        };
        ui.add_enabled(enabled, button).clicked()
    }

    // ── «Файл» ─────────────────────────────────────────────────────────
    fn file_menu(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.menu_button("Файл", |ui| {
            let idle = !self.busy();
            let has_text = !self.text.is_empty();

            if Self::menu_item(ui, "Загрузить текст…", "Ctrl+L", idle) {
                self.open_load_dialog();
                ui.close_menu();
            }
            if Self::menu_item(ui, "Сохранить текст", "Ctrl+Alt+S", idle && has_text)
            {
                self.save_text(false);
                ui.close_menu();
            }
            if Self::menu_item(ui, "Сохранить текст как…", "Ctrl+S", idle && has_text)
            {
                self.save_text(true);
                ui.close_menu();
            }
            if Self::menu_item(ui, "Очистить текстовое окно", "", idle && has_text)
            {
                self.clear_text();
                ui.close_menu();
            }
            ui.separator();
            if Self::menu_item(ui, "Загрузить основной словарь…", "", idle)
            {
                self.dialog
                    .open_for(FilePurpose::Dictionary, self.dictionary_path.as_deref());
                ui.close_menu();
            }
            if Self::menu_item(ui, "Загрузить словарь регулярных выражений…", "", idle)
            {
                self.dialog.open_for(
                    FilePurpose::RegexDictionary,
                    self.regex_dict_path.as_deref(),
                );
                ui.close_menu();
            }
            if Self::menu_item(
                ui,
                "Очистить список последних файлов",
                "",
                !self.recent.is_empty(),
            ) {
                self.recent.clear();
                ui.close_menu();
            }
            if Self::menu_item(ui, "Свойства файла…", "Ctrl+P", true) {
                self.open_props();
                ui.close_menu();
            }
            ui.separator();
            if Self::menu_item(ui, "Выход", "", true) {
                self.save_settings();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                ui.close_menu();
            }

            // ── список последних файлов, как `Openfile0…` оригинала ────
            if !self.recent.is_empty() {
                ui.separator();
                let recent = self.recent.clone();
                for (i, path) in recent.iter().enumerate() {
                    let label = format!(
                        "{}. {}",
                        i + 1,
                        path.file_name().unwrap_or_default().to_string_lossy()
                    );
                    let resp = ui
                        .add_enabled(idle, egui::Button::new(label))
                        .on_hover_text(path.display().to_string());
                    if resp.clicked() {
                        if path.is_file() {
                            self.load_path(path.clone(), None);
                        } else {
                            self.status_left = format!("файл не найден: {}", path.display());
                            self.recent.retain(|p| p != path);
                        }
                        ui.close_menu();
                    }
                }
            } else if let Some(path) = &self.current_file {
                ui.separator();
                ui.label(
                    egui::RichText::new(path.to_string_lossy().into_owned())
                        .small()
                        .color(COLOR_DIM),
                );
            }
        });
    }

    // ── «Редактировать» ────────────────────────────────────────────────
    fn edit_menu(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.menu_button("Редактировать", |ui| {
            // Ctrl+X / Ctrl+C / Ctrl+V обслуживает сам `TextEdit` (нативные
            // события egui). Пунктам меню достаточно попросить об этом
            // eframe: `RequestPaste` заставляет интеграцию прочитать системный
            // буфер обмена и прислать `Event::Paste` — отдельный `arboard`
            // не нужен.
            let idle = !self.busy();
            let has_sel = self.has_selection(ctx);
            let can_copy = has_sel;
            let can_cut = idle && has_sel;
            if Self::menu_item(ui, "Вырезать", "Ctrl+X", can_cut) {
                ctx.memory_mut(|m| m.request_focus(text_edit_id()));
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestCut);
                ui.close_menu();
            }
            if Self::menu_item(ui, "Копировать", "Ctrl+C", can_copy) {
                if let Some(text) = self.selected_text(ctx) {
                    ctx.copy_text(text);
                }
                ctx.memory_mut(|m| m.request_focus(text_edit_id()));
                ui.close_menu();
            }
            if Self::menu_item(ui, "Вставить", "Ctrl+V", idle) {
                ctx.memory_mut(|m| m.request_focus(text_edit_id()));
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                ui.close_menu();
            }
            if Self::menu_item(ui, "Выделить всё", "Ctrl+A", !self.text.is_empty()) {
                self.select_all(ctx);
                ui.close_menu();
            }
            ui.separator();
            if Self::menu_item(ui, "Откат", "Ctrl+Z", idle && !self.text.is_empty()) {
                request_undo(ctx);
                ui.close_menu();
            }
            ui.separator();
            if Self::menu_item(ui, "Найти…", "Ctrl+F", idle) {
                self.open_find(false);
                ui.close_menu();
            }
            if Self::menu_item(ui, "Найти регулярное выражение…", "Ctrl+R", idle)
            {
                self.open_find(true);
                ui.close_menu();
            }
            if Self::menu_item(ui, "Найти снова", "F3", idle && !self.find.query.is_empty())
            {
                self.find_again(ctx, true);
                ui.close_menu();
            }
            if Self::menu_item(ui, "Искать замену вперёд", "Ctrl+W", idle) {
                self.find_yofication_word(ctx, true);
                ui.close_menu();
            }
            if Self::menu_item(ui, "Искать замену назад", "Ctrl+Q", idle) {
                self.find_yofication_word(ctx, false);
                ui.close_menu();
            }
        });
    }

    // ── «Ёфикация» ─────────────────────────────────────────────────────
    fn yofication_menu(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.menu_button("Ёфикация", |ui| {
            let idle = !self.busy();
            let has_text = !self.text.is_empty();
            if Self::menu_item(ui, "Ёфицировать с текущей позиции", "F9", idle && has_text)
            {
                self.yoficate_from_cursor(ctx);
                ui.close_menu();
            }
            if Self::menu_item(
                ui,
                "Ёфицировать с начала текста",
                "Ctrl+F9",
                idle && has_text,
            ) {
                self.start();
                ui.close_menu();
            }
            ui.separator();
            if Self::menu_item(ui, "Убрать выделение", "Shift+F10", !self.marks.is_empty())
            {
                self.remove_marks();
                ui.close_menu();
            }
            if Self::menu_item(ui, "Сбросить «Всегда Е/Ё»", "", true) {
                self.reset_user_selections();
                ui.close_menu();
            }
            ui.separator();
            if Self::menu_item(ui, "Прервать", "Esc", self.busy()) {
                self.stop();
                ui.close_menu();
            }
            ui.separator();
            if Self::menu_item(ui, "Статистика…", "", true) {
                self.show_stats = true;
                ui.close_menu();
            }
        });
    }

    // ── «Настройки» ────────────────────────────────────────────────────
    fn settings_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Настройки", |ui| {
            ui.menu_button("Ёфикация", |ui| {
                self.yofication_options(ui);
            });
            ui.menu_button("Неоднозначные замены", |ui| {
                for policy in AmbiguousPolicy::ALL {
                    if ui
                        .radio(self.opts.ambiguous == policy, policy.label())
                        .clicked()
                    {
                        self.opts.ambiguous = policy;
                        ui.close_menu();
                    }
                }
            });
            ui.menu_button("Редактор", |ui| {
                self.editor_options(ui);
            });
            ui.separator();
            if Self::menu_item(ui, "Шрифт…", "", true) {
                self.open_font_dialog();
                ui.close_menu();
            }
            if Self::menu_item(ui, "Цвета подсветки…", "", true) {
                self.show_colors = true;
                ui.close_menu();
            }
            if Self::menu_item(ui, "Опции…", "Ctrl+Alt+O", true) {
                self.show_options = true;
                ui.close_menu();
            }
        });
    }

    // ── «Помощь» ───────────────────────────────────────────────────────
    fn help_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Помощь", |ui| {
            if Self::menu_item(ui, "Статистика…", "", true) {
                self.show_stats = true;
                ui.close_menu();
            }
            if Self::menu_item(ui, "Журнал замен", "", !self.journal.is_empty()) {
                self.show_journal = true;
                ui.close_menu();
            }
            if Self::menu_item(ui, "О программе", "F1", true) {
                self.show_about = true;
                ui.close_menu();
            }
        });
    }

    /// Флажки вкладки «Ёфикация» — один и тот же код для меню и окна опций.
    fn yofication_options(&mut self, ui: &mut egui::Ui) {
        let o = &mut self.opts;
        ui.checkbox(&mut o.reg_exprs, "Использовать регулярные выражения");
        ui.separator();

        // «Только безопасные» и «Только интерактивные» взаимоисключающие:
        // включённый режим блокирует второй флажок.
        let safe = ui.add_enabled(
            !o.var_only,
            egui::Checkbox::new(&mut o.no_var_only, "Только безопасные замены"),
        );
        if safe.changed() {
            o.set_no_var_only(o.no_var_only);
        }
        let interactive = ui.add_enabled(
            !o.no_var_only,
            egui::Checkbox::new(&mut o.var_only, "Только интерактивные замены"),
        );
        if interactive.changed() {
            o.set_var_only(o.var_only);
        }

        ui.separator();
        ui.checkbox(&mut o.check_yo, "Проверять расстановку «ё»");
        ui.checkbox(&mut o.always_ask, "Всегда подтверждать замену");
        ui.checkbox(&mut o.to_confirm_abbr, "…подтверждать для сокращений");
        ui.checkbox(&mut o.to_confirm_cap, "…подтверждать для имён собственных");
        ui.checkbox(
            &mut o.to_confirm_ellipsis,
            "…подтверждать для оборванных слов",
        );
        ui.separator();
        ui.checkbox(&mut o.propose_last, "Предлагать последний вариант замены");
        ui.checkbox(&mut o.fb_format, "Учитывать формат FictionBook");
        ui.checkbox(&mut o.fast_scroll, "Быстрая прокрутка");
        ui.horizontal(|ui| {
            ui.label("Строк проматки:");
            let mut n = o.lines_below as i32;
            if ui.add(egui::DragValue::new(&mut n).range(0..=20)).changed() {
                o.lines_below = n.max(0) as usize;
            }
        });
    }

    /// Флажки вкладки «Редактор».
    fn editor_options(&mut self, ui: &mut egui::Ui) {
        let o = &mut self.opts;
        ui.checkbox(&mut o.mark, "Помечать исправленное");
        ui.checkbox(&mut o.word_wrap, "Переносить текст по словам");
        ui.checkbox(&mut o.auto_unicode, "Автораспознавание Unicode");
        ui.checkbox(&mut o.last_file, "Помнить последний файл");
        ui.separator();
        ui.checkbox(&mut self.highlight, "Подсвечивать замены в тексте");
        ui.checkbox(&mut self.show_journal, "Вести журнал замен");
        ui.checkbox(&mut self.dark, "Тёмная тема");
    }

    // ── редактор ───────────────────────────────────────────────────────
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
                    if ui
                        .button("Найти")
                        .on_hover_text("Найти текст или регулярное выражение (Ctrl+F)")
                        .clicked()
                    {
                        self.open_find(false);
                    }
                    if self.highlight && !self.marks.is_empty() {
                        ui.label(
                            egui::RichText::new(format!("замен: {}", self.marks.len()))
                                .small()
                                .color(COLOR_DIM),
                        );
                    }
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
            let scroll_to = self.scroll_to_selection.take();

            let highlighted = self.highlight
                && self.opts.mark
                && !self.marks.is_empty()
                && self.text.len() <= MAX_HIGHLIGHT_CHARS;

            let marks: &[(usize, usize)] = &self.marks;
            let font = self.editor_font.clone();
            let mark_text = color32(self.mark_text);
            let mark_bg = color32(self.mark_bg);
            let mut layouter = |ui: &egui::Ui, s: &str, wrap_width: f32| {
                let mut job = highlight_job(ui, s, marks, &font, mark_text, mark_bg, highlighted);
                job.wrap.max_width = wrap_width;
                ui.fonts(|f| f.layout_job(job))
            };

            let row_height = ui.fonts(|f| f.row_height(&font.font_id())).max(1.0);
            let rows = ((ui.available_height() / row_height).floor() as usize).max(1);
            let width = if self.opts.word_wrap {
                ui.available_width()
            } else {
                f32::INFINITY
            };

            let mut changed = false;
            let mut menu: Option<EditorMenu> = None;
            let has_sel = self.has_selection(ctx);
            let can_paste = !busy;
            let can_clear = !busy && !self.text.is_empty();
            let ask_plate = color32a(self.ask_plate);
            let ask_underline = self.ask_underline;
            // Запоминаем выделение до кадра — правый клик не должен его сбрасывать
            let prev_selection = TextEditState::load(ctx, text_edit_id())
                .and_then(|s| s.cursor.char_range());
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // Текстовый редактор должен оставаться выделяемым даже при ёфикации,
                    // иначе скопировать мышью нельзя. Поэтому interactive всегда true,
                    // а правку при busy просто игнорируем.
                    let mut edit_text = self.text.clone();
                    let edit = egui::TextEdit::multiline(&mut edit_text)
                        .id(text_edit_id())
                        .font(font.font_id())
                        .desired_width(width)
                        .desired_rows(rows)
                        .frame(false)
                        .interactive(true)
                        .hint_text(
                            "Вставьте текст или загрузите файл (Файл → Загрузить текст…)",
                        )
                        // Свой расчёт вёрстки нужен всегда: только он умеет
                        // подсветку замен и синтетический курсив.
                        .layouter(&mut layouter);
                    let out = edit.show(ui);

                    // Правый клик внутри выделения не должен сбрасывать выделение
                    if ui.ctx().input(|i| i.pointer.secondary_clicked()) {
                        if let Some(prev) = prev_selection {
                            if prev.primary.index != prev.secondary.index {
                                if let Some(mut state) =
                                    TextEditState::load(ctx, text_edit_id())
                                {
                                    let cur = state.cursor.char_range();
                                    if cur.map_or(true, |r| {
                                        r.primary.index == r.secondary.index
                                    }) {
                                        state.cursor.set_char_range(Some(prev));
                                        state.store(ctx, text_edit_id());
                                    }
                                }
                            }
                        }
                    }

                    // ── контекстное меню правой кнопки ─────────────────
                    out.response.context_menu(|ui| {
                        if ui
                            .add_enabled(has_sel, egui::Button::new("Копировать"))
                            .clicked()
                        {
                            menu = Some(EditorMenu::Copy);
                            ui.close_menu();
                        }
                        if ui
                            .add_enabled(can_paste, egui::Button::new("Вставить"))
                            .clicked()
                        {
                            menu = Some(EditorMenu::Paste);
                            ui.close_menu();
                        }
                        if ui
                            .add_enabled(can_clear, egui::Button::new("Очистить всё"))
                            .clicked()
                        {
                            menu = Some(EditorMenu::Clear);
                            ui.close_menu();
                        }
                    });

                    changed = out.response.changed();
                    if changed && !busy {
                        self.text = edit_text;
                    } else if changed && busy {
                        // Правку во время ёфикации игнорируем, но сохраняем
                        // выделение (оно уже в TextEditState)
                    }
                    if let Some(range) = ask_word {
                        // `TextEdit` рисует выделение только в фокусе, а
                        // read-only редактор его не получает — подсвечиваем
                        // слово сами, как `Editor.SelLength` оригинала.
                        paint_ask_word(ui, &out, range, ask_plate, ask_underline);
                        if scroll_now {
                            let cursor = out.galley.from_ccursor(CCursor::new(range.0));
                            let rect = out
                                .galley
                                .pos_from_cursor(&cursor)
                                .translate(out.galley_pos.to_vec2());
                            ui.scroll_to_rect(rect, Some(egui::Align::Center));
                        }
                    }
                    if let Some(pos) = scroll_to {
                        let cursor = out.galley.from_ccursor(CCursor::new(pos));
                        let rect = out
                            .galley
                            .pos_from_cursor(&cursor)
                            .translate(out.galley_pos.to_vec2());
                        ui.scroll_to_rect(rect, Some(egui::Align::Center));
                    }
                });
            if scroll_now {
                self.scroll_to_ask = false;
            }
            match menu {
                Some(EditorMenu::Copy) => {
                    if let Some(text) = self.selected_text(ctx) {
                        ctx.copy_text(text);
                        self.status_left = "скопировано".to_owned();
                    }
                }
                Some(EditorMenu::Paste) => {
                    ctx.memory_mut(|m| m.request_focus(text_edit_id()));
                    ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                }
                Some(EditorMenu::Clear) => self.clear_text(),
                None => {}
            }

            if changed && !busy {
                // Ручная правка делает пометки неверными (во время ёфикации правка игнорируется)
                self.marks.clear();
                self.text_chars = self.text.chars().count();
            }
        });
    }

    /// Текст выделения редактора (для «Копировать» из контекстного меню).
    fn selected_text(&self, ctx: &egui::Context) -> Option<String> {
        let r = TextEditState::load(ctx, text_edit_id())?
            .cursor
            .char_range()?;
        let (a, b) = (
            r.primary.index.min(r.secondary.index),
            r.primary.index.max(r.secondary.index),
        );
        if a == b {
            return None;
        }
        Some(self.text.chars().skip(a).take(b - a).collect())
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
        // Фокус не требуем: меню и контекстное меню забирают его у редактора,
        // но выделение при этом остаётся.
        TextEditState::load(ctx, text_edit_id())
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
}

// ═══════════════════════════════════════════════════════════════════════════
//  Шрифты и стиль
// ═══════════════════════════════════════════════════════════════════════════

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
//  Мелкие помощники интерфейса
// ═══════════════════════════════════════════════════════════════════════════

/// Время изменения файла в виде `ГГГГ-ММ-ДД ЧЧ:ММ`.
fn format_system_time(t: SystemTime) -> String {
    let secs = match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    // Гражданский календарь из номера дня эпохи (алгоритм Ховарда Хиннанта).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096).div_euclid(365);
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02} {h:02}:{m:02}")
}

/// Пара «е/ё» для слова по словарю: `(вариант с Е, вариант с Ё)` в том же
/// регистре, что и слово в тексте.
fn dictionary_variants(dict: &Dictionary, word: &str) -> Option<(String, String)> {
    let mut a = String::new();
    let mut b = String::new();
    dict.find_ul(word, &mut a, &mut b)
        .map(|i| (copy_case(dict.ye(i), word), copy_case(dict.yo(i), word)))
}

/// Сработало ли сочетание клавиш (с погашением события).
fn shortcut(ctx: &egui::Context, modifiers: egui::Modifiers, key: egui::Key) -> bool {
    ctx.input_mut(|i| i.consume_shortcut(&egui::KeyboardShortcut::new(modifiers, key)))
}

/// Просьба к `TextEdit` откатить последнее изменение (`Ctrl+Z` из меню).
///
/// `TextEditState::undoer` закрыт, поэтому единственный публичный путь —
/// подсунуть редактору то же событие, которое он получает от клавиатуры.
fn request_undo(ctx: &egui::Context) {
    ctx.memory_mut(|m| m.request_focus(text_edit_id()));
    ctx.input_mut(|i| {
        i.events.push(egui::Event::Key {
            key: egui::Key::Z,
            physical_key: Some(egui::Key::Z),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
    });
}

/// Байтовое смещение символа с индексом `index`.
fn char_to_byte(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

/// Индекс символа, в котором начинается байт `byte`.
fn byte_to_char(text: &str, byte: usize) -> usize {
    text.char_indices().take_while(|(b, _)| *b < byte).count()
}

/// Поиск подстроки, начиная с символа `from`; индексы — в символах.
///
/// Поиск зациклен: не найдя дальше, начинаем с начала (как «Найти снова»
/// в Delphi-редакторе).
fn text_find(
    text: &str,
    needle: &str,
    from: usize,
    forward: bool,
    case_sensitive: bool,
) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let hay: Vec<char> = text.chars().collect();
    let pat: Vec<char> = needle.chars().collect();
    if pat.len() > hay.len() {
        return None;
    }
    let eq = |a: char, b: char| {
        if case_sensitive {
            a == b
        } else {
            a.to_lowercase().eq(b.to_lowercase())
        }
    };
    let matches = |at: usize| (0..pat.len()).all(|k| eq(hay[at + k], pat[k]));
    let last = hay.len() - pat.len();

    if forward {
        let start = from.min(hay.len());
        for at in (start..=last).chain(0..start.min(last + 1)) {
            if matches(at) {
                return Some((at, at + pat.len()));
            }
        }
    } else {
        let end = from.min(last + 1);
        for at in (0..end).rev().chain((end..=last).rev()) {
            if matches(at) {
                return Some((at, at + pat.len()));
            }
        }
    }
    None
}

/// Поиск регулярного выражения; индексы — в символах.
fn regex_find(
    text: &str,
    pattern: &str,
    from: usize,
    forward: bool,
    case_sensitive: bool,
) -> Result<Option<(usize, usize)>, String> {
    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|e| e.to_string())?;
    let from_byte = char_to_byte(text, from);
    let found = if forward {
        re.find_at(text, from_byte).or_else(|| re.find(text))
    } else {
        let mut before = None;
        for m in re.find_iter(text) {
            if m.start() < from_byte {
                before = Some(m);
            } else {
                break;
            }
        }
        before.or_else(|| re.find_iter(text).last())
    };
    Ok(found.map(|m| (byte_to_char(text, m.start()), byte_to_char(text, m.end()))))
}

/// `Rgb` из настроек → цвет egui.
fn color32(c: Rgb) -> egui::Color32 {
    egui::Color32::from_rgb(c.r, c.g, c.b)
}

/// `Rgba` из настроек → цвет egui.
fn color32a(c: Rgba) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

/// Цвет egui → компоненты для `yo.ini`.
fn rgb_of(c: egui::Color32) -> Rgb {
    let [r, g, b, _] = c.to_srgba_unmultiplied();
    Rgb::new(r, g, b)
}

/// Цвет с прозрачностью → компоненты для `yo.ini`.
fn rgba_of(c: egui::Color32) -> Rgba {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    Rgba::new(r, g, b, a)
}

impl Drop for YoApp {
    /// Последний шанс сохранить `yo.ini`: eframe уничтожает приложение при
    /// закрытии окна, и это единственный момент, общий для всех путей выхода.
    fn drop(&mut self) {
        self.save_settings();
    }
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
        self.handle_shortcuts(ctx);

        self.menu_bar(ctx);
        // Нижние панели накладываются в порядке вызова: первая — самая нижняя.
        self.status_bar(ctx);
        if self.ask.is_some() {
            self.ask_panel(ctx);
        }
        self.editor_panel(ctx);

        self.file_dialog_window(ctx);
        self.find_window(ctx);
        self.font_window(ctx);
        self.colors_window(ctx);
        self.props_window(ctx);
        self.options_window(ctx);
        self.about_window(ctx);
        self.stats_window(ctx);
        if self.show_journal {
            self.journal_window(ctx);
        }
    }
}

/// Строка таблицы «ключ — значение» в окне статистики.
fn stats_row(ui: &mut egui::Ui, key: &str, value: String) {
    ui.label(key);
    ui.label(egui::RichText::new(value).monospace());
    ui.end_row();
}

/// `LayoutJob` для единого редактора: текст выбранным шрифтом плюс
/// подсветка изменённых букв.
///
/// Текст обязан остаться ровно тем же, что и в буфере (никаких вставок),
/// иначе `TextEdit` потеряет соответствие курсора и строк.
fn highlight_job(
    ui: &egui::Ui,
    text: &str,
    marks: &[(usize, usize)],
    font: &EditorFont,
    mark_text: egui::Color32,
    mark_bg: egui::Color32,
    highlighted: bool,
) -> egui::text::LayoutJob {
    let normal = egui::TextFormat {
        font_id: font.font_id(),
        // Тот же цвет, что берёт сам `TextEdit` по умолчанию.
        color: ui.visuals().widgets.inactive.text_color(),
        italics: font.italics,
        ..Default::default()
    };
    let mut job = egui::text::LayoutJob::default();
    if !highlighted {
        job.append(text, 0.0, normal);
        return job;
    }
    let mut hot = normal.clone();
    hot.color = mark_text;
    hot.background = mark_bg;

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
    plate: egui::Color32,
    underline: f32,
) {
    let galley = &out.galley;
    if start >= end || end > galley.text().chars().count() {
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
        paint_word_rect(&painter, rect, plate, underline);
    } else {
        // Слово переносится: подсвечиваем хвост первой строки, все
        // промежуточные целиком и начало последней.
        let rect = egui::Rect::from_min_max(
            egui::pos2(caret_start.left(), caret_start.top()),
            egui::pos2(galley.rect.right() + out.galley_pos.x, caret_start.bottom()),
        );
        paint_word_rect(&painter, rect, plate, underline);
        for row in galley.rows.iter() {
            let row_rect = row.rect.translate(out.galley_pos.to_vec2());
            if row_rect.center().y <= first + 0.5 || row_rect.center().y >= last - 0.5 {
                continue;
            }
            paint_word_rect(&painter, row_rect, plate, underline);
        }
        let rect = egui::Rect::from_min_max(
            egui::pos2(out.galley_pos.x, caret_end.top()),
            egui::pos2(caret_end.left(), caret_end.bottom()),
        );
        paint_word_rect(&painter, rect, plate, underline);
    }
}

/// Плашка + подчёркивание: заметно, но текст остаётся читаемым.
fn paint_word_rect(
    painter: &egui::Painter,
    rect: egui::Rect,
    plate: egui::Color32,
    underline: f32,
) {
    painter.rect_filled(rect.expand2(egui::vec2(1.0, 1.0)), 2.0, plate);
    if underline > 0.0 {
        painter.hline(
            rect.x_range(),
            rect.bottom(),
            egui::Stroke::new(underline, COLOR_MARK),
        );
    }
}

/// `LayoutJob` строки контекста для панели запроса.
///
/// Показывает `до` + спорное слово + `после`; спорное слово — цветом и
/// плашкой из настроек, остальное — приглушённо. Слишком длинные «до»/«после»
/// обрезаются, чтобы слово всегда оставалось видимым: берём последние
/// [`CONTEXT_BEFORE`] и первые [`CONTEXT_AFTER`] символов.
fn ask_context_job(
    ask: &AskRequest,
    ask_text: egui::Color32,
    plate: egui::Color32,
) -> egui::text::LayoutJob {
    const CONTEXT_BEFORE: usize = 64;
    const CONTEXT_AFTER: usize = 64;

    let font = egui::FontId::monospace(13.0);
    let dim = egui::TextFormat {
        font_id: font.clone(),
        color: COLOR_DIM,
        ..Default::default()
    };
    let hot = egui::TextFormat {
        font_id: font,
        color: ask_text,
        background: plate,
        underline: egui::Stroke::new(1.0_f32, ask_text),
        ..Default::default()
    };

    let mut job = egui::text::LayoutJob::default();
    let before: String = ask
        .before
        .chars()
        .rev()
        .take(CONTEXT_BEFORE)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if ask.before.chars().count() > CONTEXT_BEFORE {
        job.append("…", 0.0, dim.clone());
    }
    if !before.is_empty() {
        job.append(&before, 0.0, dim.clone());
    }
    job.append(&ask.word1, 0.0, hot);
    let after: String = ask.after.chars().take(CONTEXT_AFTER).collect();
    if !after.is_empty() {
        job.append(&after, 0.0, dim.clone());
    }
    if ask.after.chars().count() > CONTEXT_AFTER {
        job.append("…", 0.0, dim);
    }
    job
}

/// Тело панели запроса: рамка с заголовком и ряд из шести кнопок.
///
/// Вынесено из метода, чтобы вёрстку можно было проверить в тестах без окна
/// (см. `mod gui_tests`). Возвращает нажатое решение и прямоугольник ряда —
/// по нему тест убеждается, что кластер стоит ровно по центру.
fn ask_panel_ui(
    ui: &mut egui::Ui,
    ask: &AskRequest,
    ask_text: egui::Color32,
    plate: egui::Color32,
) -> (Option<AskDecision>, egui::Rect) {
    let title = ask_title(ask);
    let title_color = if ask.wrong_yo || ask.replaced_yo || ask.wrong_ye {
        COLOR_ALERT
    } else {
        COLOR_YE
    };
    let mut decision = None;
    let mut row = egui::Rect::NOTHING;

    ui.add_space(6.0);
    // Живой контекст слова — во всю длину панели и по центру, как на скриншоте:
    // «хотя мы всё ещё не…» должно быть посередине, а не прижато к левому краю.
    // Делаем Label selectable, чтобы его тоже можно было скопировать.
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), 18.0),
        egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
        |ui| {
            ui.add(
                egui::Label::new(ask_context_job(ask, ask_text, plate))
                    .halign(egui::Align::Center)
                    .selectable(true)
                    .truncate(),
            );
        },
    );
    ui.add_space(4.0);
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
        let title = self.dialog.title();
        let is_text = self.dialog.purpose == FilePurpose::Text;

        let mut action: Option<FileAction> = None;
        let mut new_dir: Option<PathBuf> = None;

        egui::Window::new(title)
            .open(&mut open)
            .resizable(true)
            .default_size([640.0, 460.0])
            .show(ctx, |ui| {
                // ── диски и быстрые переходы ───────────────────────────────
                ui.horizontal_wrapped(|ui| {
                    ui.label("Диск:");
                    for (label, path) in system_roots() {
                        let here = self.dialog.dir == path;
                        if ui.selectable_label(here, label).clicked() {
                            new_dir = Some(path);
                        }
                    }
                    if ui.button("↑").on_hover_text("На уровень выше").clicked() {
                        if let Some(p) = self.dialog.dir.parent() {
                            new_dir = Some(p.to_path_buf());
                        }
                    }
                });

                // ── путь ───────────────────────────────────────────────
                ui.horizontal(|ui| {
                    ui.label("Папка:");
                    let mut dir_str = self.dialog.dir.to_string_lossy().to_string();
                    let r = ui.add_sized(
                        [ui.available_width(), 20.0],
                        egui::TextEdit::singleline(&mut dir_str),
                    );
                    if r.lost_focus() {
                        new_dir = Some(PathBuf::from(dir_str));
                    }
                });

                // ── имя и кодировка ────────────────────────────────────
                ui.horizontal(|ui| {
                    ui.label("Имя:");
                    let width = if is_text {
                        ui.available_width() - 190.0
                    } else {
                        ui.available_width()
                    };
                    let r = ui.add_sized(
                        [width.max(120.0), 20.0],
                        egui::TextEdit::singleline(&mut self.dialog.name),
                    );
                    if is_text {
                        ui.label("Кодировка:");
                        ui.menu_button(self.dialog.encoding.label(), |ui| {
                            for enc in TextEncoding::ALL {
                                ui.selectable_value(&mut self.dialog.encoding, enc, enc.label());
                            }
                        });
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        action = Some(if self.dialog.is_save {
                            FileAction::Save(self.dialog.path(), self.dialog.encoding)
                        } else {
                            self.dialog_action_for(self.dialog.path())
                        });
                    }
                });
                ui.separator();

                // ── список ─────────────────────────────────────────────
                let exts: Option<Vec<&'static str>> = if is_text {
                    None
                } else {
                    Some(self.dialog.purpose.extensions().to_vec())
                };
                if !self.dialog.dir.is_dir() {
                    ui.colored_label(COLOR_MARK, "нет такой папки");
                    new_dir = Some(std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
                }
                let entries = list_dir_filtered(&self.dialog.dir, exts.as_deref());
                let purpose = self.dialog.purpose;
                let is_save = self.dialog.is_save;
                let dir = self.dialog.dir.clone();
                let enc = self.dialog.encoding;
                ui.push_id("file_list", |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .max_height(ui.available_height() - 40.0)
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
                                        new_dir = Some(dir.join(&name));
                                    } else {
                                        let path = dir.join(&name);
                                        action = Some(if is_save {
                                            FileAction::Save(path, enc)
                                        } else {
                                            match purpose {
                                                FilePurpose::Text => FileAction::Load(path, enc),
                                                FilePurpose::Dictionary => {
                                                    FileAction::OpenDictionary(path)
                                                }
                                                FilePurpose::RegexDictionary => {
                                                    FileAction::OpenRegexDictionary(path)
                                                }
                                                FilePurpose::Font => FileAction::OpenFont(path),
                                            }
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
                    let ok = if is_save {
                        "Сохранить"
                    } else if purpose == FilePurpose::Text {
                        "Открыть"
                    } else {
                        "Загрузить"
                    };
                    if ui.button(ok).clicked() {
                        action = Some(if is_save {
                            FileAction::Save(self.dialog.path(), self.dialog.encoding)
                        } else {
                            self.dialog_action_for(self.dialog.path())
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
            if d.is_dir() {
                self.dialog.dir = d;
                self.dialog.refresh();
            } else {
                self.dialog.status = "это не папка".to_owned();
            }
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
                FileAction::OpenDictionary(p) => {
                    if let Some(dict) = load_dictionary(&p) {
                        self.dict_override = Some(dict);
                        self.dictionary_path = Some(p.clone());
                        self.status_left =
                            format!("основной словарь: {} слов ({})", dict.len(), dict.version());
                        open = false;
                    } else {
                        self.dialog.status = "не словарь YOX".to_owned();
                    }
                }
                FileAction::OpenRegexDictionary(p) => {
                    if let Some(re) = load_regex_dictionary(&p) {
                        self.re_override = Some(re);
                        self.regex_dict_path = Some(p.clone());
                        self.status_left = format!(
                            "словарь регулярных выражений: {} правил ({})",
                            re.len(),
                            re.version()
                        );
                        open = false;
                    } else {
                        self.dialog.status = "не словарь регулярных выражений".to_owned();
                    }
                }
                FileAction::OpenFont(p) => {
                    if p.is_file() {
                        self.adopt_font_file(p);
                        open = false;
                    } else {
                        self.dialog.status = "это не файл".to_owned();
                    }
                }
            }
        }
        self.dialog.open = open;
    }

    /// Действие «Открыть» для текущего назначения диалога.
    fn dialog_action_for(&self, path: PathBuf) -> FileAction {
        if self.dialog.is_save {
            return FileAction::Save(path, self.dialog.encoding);
        }
        match self.dialog.purpose {
            FilePurpose::Text => FileAction::Load(path, self.dialog.encoding),
            FilePurpose::Dictionary => FileAction::OpenDictionary(path),
            FilePurpose::RegexDictionary => FileAction::OpenRegexDictionary(path),
            FilePurpose::Font => FileAction::OpenFont(path),
        }
    }

    // ────────────────────────────────────────────────────────────────────
    //  Поиск
    // ────────────────────────────────────────────────────────────────────

    fn find_window(&mut self, ctx: &egui::Context) {
        if !self.show_find {
            return;
        }
        let mut open = true;
        let mut close = false;
        let mut forward = false;
        let mut backward = false;
        egui::Window::new(if self.find.regex {
            "Найти регулярное выражение"
        } else {
            "Найти"
        })
        .open(&mut open)
        .resizable(false)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Что найти:");
                let r = ui.add_sized(
                    [260.0, 20.0],
                    egui::TextEdit::singleline(&mut self.find.query),
                );
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    forward = true;
                }
            });
            ui.checkbox(&mut self.find.regex, "Регулярное выражение");
            ui.checkbox(&mut self.find.case_sensitive, "Учитывать регистр");
            if let Some(err) = &self.find.error {
                ui.colored_label(COLOR_MARK, err.as_str());
            }
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Найти вперёд").clicked() {
                    forward = true;
                }
                if ui.button("Найти назад").clicked() {
                    backward = true;
                }
                if ui.button("Закрыть").clicked() {
                    close = true;
                }
            });
        });
        if close {
            open = false;
        }
        if forward {
            self.find_again(ctx, true);
        }
        if backward {
            self.find_again(ctx, false);
        }
        self.show_find = open;
    }

    // ────────────────────────────────────────────────────────────────────
    //  Шрифт
    // ────────────────────────────────────────────────────────────────────

    fn open_font_dialog(&mut self) {
        self.show_font_dialog = true;
    }

    /// Применить `self.font_spec` к контексту (пересобрать шрифты).
    fn apply_fonts(&mut self, ctx: &egui::Context) {
        self.font_spec = self.font_spec.clone().sanitized();
        self.editor_font = install_fonts(ctx, &self.font_spec);
        ctx.request_repaint();
    }

    /// Взять шрифт из файла: семейство — имя файла, начертание — по имени.
    fn adopt_font_file(&mut self, path: PathBuf) {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Свой шрифт".to_owned());
        let lc = name.to_lowercase();
        let bold = lc.contains("bold") || lc.contains("black") || lc.contains("heavy");
        let italic = lc.contains("italic") || lc.contains("oblique");
        self.font_spec.file = Some(path);
        self.font_spec.family = name;
        self.font_spec.style = match (bold, italic) {
            (true, true) => FontStyle::BoldItalic,
            (true, false) => FontStyle::Bold,
            (false, true) => FontStyle::Italic,
            (false, false) => FontStyle::Regular,
        };
        self.status_left = format!("шрифт редактора: {}", self.font_spec.family);
    }

    fn font_window(&mut self, ctx: &egui::Context) {
        if !self.show_font_dialog {
            return;
        }
        // Список системных шрифтов читается один раз — при первом открытии
        // диалога, а не на старте: приложение не должно зависеть от того,
        // сколько шрифтов установлено.
        if !self.font_catalog_scanned {
            self.font_catalog = FontCatalog::scan();
            self.font_catalog_scanned = true;
        }
        let mut open = true;
        let mut close = false;
        let mut changed = false;

        egui::Window::new("Шрифт редактора")
            .open(&mut open)
            .resizable(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Семейство:");
                    let current = self.font_spec.family.clone();
                    egui::ComboBox::from_id_salt("font_family")
                        .selected_text(current.clone())
                        .width(240.0)
                        .show_ui(ui, |ui| {
                            for (name, hint) in [
                                ("Hack", "моноширинный, встроенный"),
                                ("Ubuntu Light", "пропорциональный, встроенный"),
                            ] {
                                let selected = self.font_spec.family == name;
                                if ui
                                    .selectable_label(selected, format!("{name} — {hint}"))
                                    .clicked()
                                {
                                    self.font_spec.family = name.to_owned();
                                    self.font_spec.file = None;
                                    changed = true;
                                }
                            }
                            if self.font_spec.file.is_some() {
                                let _ = ui.selectable_label(true, current);
                            }
                        });
                });
                if self.font_catalog_scanned {
                    ui.horizontal(|ui| {
                        ui.label("Установленные:");
                        let families = self.font_catalog.families();
                        egui::ComboBox::from_id_salt("font_system")
                            .selected_text(if families.is_empty() {
                                "не найдено"
                            } else {
                                "выбрать…"
                            })
                            .width(240.0)
                            .show_ui(ui, |ui| {
                                for family in families {
                                    if ui.selectable_label(false, &family).clicked() {
                                        let style = self.font_spec.style;
                                        if let Some(face) = self.font_catalog.best(&family, style) {
                                            self.font_spec.family = family.clone();
                                            self.font_spec.file = Some(face.path.clone());
                                            changed = true;
                                        }
                                    }
                                }
                            });
                    });
                }
                ui.horizontal(|ui| {
                    ui.label("Начертание:");
                    egui::ComboBox::from_id_salt("font_style")
                        .selected_text(self.font_spec.style.label())
                        .width(240.0)
                        .show_ui(ui, |ui| {
                            for style in FontStyle::ALL {
                                if ui
                                    .selectable_label(self.font_spec.style == style, style.label())
                                    .clicked()
                                {
                                    self.font_spec.style = style;
                                    // Если семейство взято из системного
                                    // каталога, подбираем файл нужного
                                    // начертания (обычный/курсив/жирный).
                                    if let Some(face) =
                                        self.font_catalog.best(&self.font_spec.family, style)
                                    {
                                        self.font_spec.file = Some(face.path.clone());
                                    }
                                    changed = true;
                                }
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("Размер:");
                    let mut size = self.font_spec.size;
                    if ui
                        .add(
                            egui::Slider::new(&mut size, MIN_FONT_SIZE..=MAX_FONT_SIZE)
                                .step_by(0.5)
                                .suffix(" пт"),
                        )
                        .changed()
                    {
                        self.font_spec.size = size;
                        changed = true;
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Загрузить из файла…").clicked() {
                        self.dialog.open_for(FilePurpose::Font, None);
                    }
                    if self.font_spec.file.is_some() && ui.button("Встроенный шрифт").clicked()
                    {
                        self.font_spec.file = None;
                        self.font_spec.family = "Hack".to_owned();
                        self.font_spec.style = FontStyle::Regular;
                        changed = true;
                    }
                });
                if let Some(file) = &self.font_spec.file {
                    ui.label(
                        egui::RichText::new(format!("файл: {}", file.display()))
                            .small()
                            .color(COLOR_DIM),
                    );
                }
                ui.separator();
                // Живой пример — тем же шрифтом, что и в редакторе.
                let preview = egui::RichText::new(
                    "Съешь же ещё этих мягких французских булок, да выпей чаю. № 42",
                )
                .font(self.editor_font.font_id());
                ui.label(preview);
                ui.separator();
                if ui.button("Закрыть").clicked() {
                    close = true;
                }
            });

        if changed {
            self.apply_fonts(ctx);
        }
        self.show_font_dialog = open && !close;
    }

    // ────────────────────────────────────────────────────────────────────
    //  Цвета подсветки
    // ────────────────────────────────────────────────────────────────────

    fn colors_window(&mut self, ctx: &egui::Context) {
        if !self.show_colors {
            return;
        }
        let mut open = true;
        let mut close = false;
        egui::Window::new("Цвета подсветки")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                let mut text = color32(self.mark_text);
                let mut bg = color32(self.mark_bg);
                let mut ask = color32(self.ask_text);
                let mut plate = color32a(self.ask_plate);

                ui.horizontal(|ui| {
                    ui.label("Изменённые буквы:");
                    if ui.color_edit_button_srgba(&mut text).changed() {
                        self.mark_text = rgb_of(text);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Плашка под ними:");
                    if ui.color_edit_button_srgba(&mut bg).changed() {
                        self.mark_bg = rgb_of(bg);
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Спорное слово:");
                    if ui.color_edit_button_srgba(&mut ask).changed() {
                        self.ask_text = rgb_of(ask);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Плашка запроса:");
                    if ui.color_edit_button_srgba(&mut plate).changed() {
                        self.ask_plate = rgba_of(plate);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Толщина подчёркивания:");
                    ui.add(
                        egui::Slider::new(&mut self.ask_underline, 0.0..=6.0)
                            .step_by(0.25)
                            .suffix(" пт"),
                    );
                });
                ui.separator();
                if ui.button("По умолчанию").clicked() {
                    let d = GuiSettings::default();
                    self.mark_text = d.mark_text;
                    self.mark_bg = d.mark_bg;
                    self.ask_text = d.ask_text;
                    self.ask_plate = d.ask_plate;
                    self.ask_underline = d.ask_underline;
                }
                if ui.button("Закрыть").clicked() {
                    close = true;
                }
            });
        self.show_colors = open && !close;
    }

    // ────────────────────────────────────────────────────────────────────
    //  Свойства файла
    // ────────────────────────────────────────────────────────────────────

    fn props_window(&mut self, ctx: &egui::Context) {
        if !self.show_props {
            return;
        }
        let mut open = true;
        let props = self.props.clone();
        egui::Window::new("Свойства файла")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                let Some(p) = props else {
                    ui.label("файл не загружен");
                    return;
                };
                egui::Grid::new("props_grid")
                    .num_columns(2)
                    .spacing([16.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("Файл:");
                        ui.label(
                            p.path
                                .as_ref()
                                .map(|x| x.display().to_string())
                                .unwrap_or_else(|| "—".to_owned()),
                        );
                        ui.end_row();
                        ui.label("Существует:");
                        ui.label(if p.exists { "да" } else { "нет" });
                        ui.end_row();
                        ui.label("Размер:");
                        ui.label(format!("{} Б", p.size));
                        ui.end_row();
                        ui.label("Изменён:");
                        ui.label(match p.modified {
                            Some(t) => format_system_time(t),
                            None => "—".to_owned(),
                        });
                        ui.end_row();
                        ui.label("Кодировка:");
                        ui.label(p.encoding.label());
                        ui.end_row();
                        ui.label("Символов:");
                        ui.label(p.chars.to_string());
                        ui.end_row();
                        ui.label("Слов:");
                        ui.label(p.words.to_string());
                        ui.end_row();
                        ui.label("Строк:");
                        ui.label(p.lines.to_string());
                        ui.end_row();
                    });
            });
        self.show_props = open;
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
            .default_size([560.0, 520.0])
            .show(ctx, |ui| {
                ui.heading("Ёфикация");
                self.yofication_options(ui);
                ui.horizontal(|ui| {
                    ui.label("Неоднозначные слова:");
                    for policy in AmbiguousPolicy::ALL {
                        ui.radio_value(&mut self.opts.ambiguous, policy, policy.label());
                    }
                });

                ui.separator();
                ui.heading("Редактор");
                self.editor_options(ui);

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Шрифт…").clicked() {
                        self.show_font_dialog = true;
                    }
                    if ui.button("Цвета подсветки…").clicked() {
                        self.show_colors = true;
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("По умолчанию").clicked() {
                        let d = GuiSettings::default();
                        self.opts = d.opts;
                        self.dark = d.dark;
                        self.highlight = d.highlight;
                        self.show_journal = d.show_journal;
                        self.mark_text = d.mark_text;
                        self.mark_bg = d.mark_bg;
                        self.ask_text = d.ask_text;
                        self.ask_plate = d.ask_plate;
                        self.ask_underline = d.ask_underline;
                        self.font_spec = d.font;
                        self.apply_fonts(ctx);
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
        // Активные базы: встроенные или загруженные пользователем.
        let (words, dic_version, rules, re_version) = {
            let dict = self.dict_override.unwrap_or_else(yol::yo_core::dictionary);
            let re = self
                .re_override
                .unwrap_or_else(yol::yo_core::regex_dictionary);
            (
                dict.len(),
                dict.version().to_owned(),
                re.len(),
                re.version().to_owned(),
            )
        };
        let ms = self.dict_info.as_ref().map(|v| v.4).unwrap_or(0);
        let _ = database_info;
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
    /// Двойной клик по `yol.exe` на Windows и запуск из ярлыка
    /// (`.desktop`, `.lnk`) не должны попадать в консольный режим,
    /// хотя `stdin.is_terminal()` там тоже `false` (у GUI-программы
    /// вообще нет консоли). Поэтому смотрим не просто «не терминал»,
    /// а именно «труба/файл» через [`stdin_kind`].
    fn is_cli(&self) -> bool {
        self.is_cli_with(stdin_kind())
    }

    /// Вариант [`is_cli`] с явным видом stdin — удобен для тестов.
    fn is_cli_with(&self, kind: StdinKind) -> bool {
        !self.help
            && !self.version
            && !self.gui
            && (self.input.is_some()
                || self.output.is_some()
                || matches!(kind, StdinKind::Pipe | StdinKind::File))
    }
}

/// Куда подключён stdin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StdinKind {
    /// Интерактивный терминал.
    Terminal,
    /// `echo foo | yol` или `yol < file`.
    Pipe,
    /// Перенаправление из файла (`< file` на некоторых системах даёт File).
    File,
    /// Нет консоли и не труба/файл — двойной клик, ярлык, .desktop без терминала.
    Other,
}

/// Определить вид stdin без блокировки.
///
/// На Unix — по `is_terminal()` и метаданным `/proc/self/fd/0`/`/dev/stdin`,
/// на Windows — по `GetFileType`/`GetConsoleMode`.
fn stdin_kind() -> StdinKind {
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return StdinKind::Terminal;
    }
    #[cfg(unix)]
    {
        return stdin_kind_unix();
    }
    #[cfg(windows)]
    {
        return stdin_kind_windows();
    }
    #[cfg(not(any(unix, windows)))]
    {
        return StdinKind::Other;
    }
}

#[cfg(unix)]
fn stdin_kind_unix() -> StdinKind {
    use std::os::unix::fs::FileTypeExt;
    // Проверяем оба пути: /proc/self/fd/0 точнее, /dev/stdin — запасной.
    for path in ["/proc/self/fd/0", "/dev/stdin"] {
        if let Ok(meta) = std::fs::metadata(path) {
            let ft = meta.file_type();
            if ft.is_fifo() {
                return StdinKind::Pipe;
            }
            if ft.is_file() {
                return StdinKind::File;
            }
            // Сокет тоже встречается при перенаправлении через сеть
            #[cfg(target_os = "linux")]
            {
                if ft.is_socket() {
                    return StdinKind::Pipe;
                }
            }
            if ft.is_char_device() {
                // /dev/null, /dev/zero и т.п. — не считаем CLI
                // (сюда попадает и двойной клик без консоли)
                continue;
            }
        }
    }
    // Fallback: если не смогли определить, но не терминал — считаем Other,
    // чтобы не открывать консольный режим на двойной клик.
    StdinKind::Other
}

#[cfg(windows)]
mod win_stdin {
    extern "system" {
        pub fn GetFileType(hFile: *mut std::ffi::c_void) -> u32;
        pub fn GetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, lpMode: *mut u32) -> i32;
    }
}

#[cfg(windows)]
fn stdin_kind_windows() -> StdinKind {
    use std::os::windows::io::AsRawHandle;
    let handle = std::io::stdin().as_raw_handle();
    if handle.is_null() {
        return StdinKind::Other;
    }
    // FILE_TYPE_* из WinBase.h
    const FILE_TYPE_UNKNOWN: u32 = 0;
    const FILE_TYPE_DISK: u32 = 1;
    const FILE_TYPE_CHAR: u32 = 2;
    const FILE_TYPE_PIPE: u32 = 3;
    unsafe {
        let ft = win_stdin::GetFileType(handle as *mut _);
        match ft {
            FILE_TYPE_PIPE => StdinKind::Pipe,
            FILE_TYPE_DISK => StdinKind::File,
            FILE_TYPE_CHAR => {
                // Консоль — уже отсеяли is_terminal, но проверим на всякий случай
                let mut mode: u32 = 0;
                if win_stdin::GetConsoleMode(handle as *mut _, &mut mode as *mut _) != 0 {
                    StdinKind::Terminal
                } else {
                    // NUL, CON и прочие char-устройства без консоли
                    StdinKind::Other
                }
            }
            FILE_TYPE_UNKNOWN | _ => StdinKind::Other,
        }
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
                               yo (ставить Ё, по умолчанию в консоли) |
                               ye (оставить Е) | ask (спрашивать)

ИНТЕРФЕЙС
    Настройки читаются из yo.ini рядом с программой (YOL_INI задаёт другой
    путь) и сохраняются при выходе. В графическом режиме неоднозначные
    слова по умолчанию требуют ответа («Спрашивать»).

    Ctrl+L           загрузить текст          Ctrl+P  свойства файла
    Ctrl+S           сохранить как…           Ctrl+Alt+S  сохранить
    F9 / Ctrl+F9     ёфицировать с позиции / с начала
    Shift+F10        убрать подсветку замен
    Ctrl+F/R, F3     найти / найти регэксп / найти снова
    Ctrl+W / Ctrl+Q  искать замену вперёд / назад
    Ctrl+O           опции                    Esc  прервать ёфикацию
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
    fn yofication_can_start_from_the_cursor() {
        let mut app = app();
        let src = "Актер вышел на сцену. Актер снова вышел.";
        app.text = src.to_owned();
        app.text_chars = app.text.chars().count();
        let from = src.find("Актер снова").expect("вторая половина");
        app.start_at(from);
        assert!(app.busy(), "прогон не начался");
        wait(&mut app);

        assert_eq!(
            app.text, "Актер вышел на сцену. Актёр снова вышел.",
            "текст до курсора должен остаться нетронутым"
        );
    }

    #[test]
    fn remove_marks_and_clear_text_reset_the_view() {
        let mut app = app();
        app.text = "Актер вышел.".to_owned();
        app.text_chars = app.text.chars().count();
        app.marks.push((0, 5));
        app.journal.push(("Актер".into(), "Актёр".into()));
        app.remove_marks();
        assert!(app.marks.is_empty());

        app.clear_text();
        assert!(app.text.is_empty());
        assert_eq!(app.text_chars, 0);
        assert!(app.journal.is_empty());
        assert!(app.current_file.is_none());
    }

    /// Крутит `pump()`, пока движок не задаст вопрос.
    fn wait_for_ask(app: &mut YoApp) -> AskRequest {
        let deadline = Instant::now() + Duration::from_secs(20);
        while app.ask.is_none() && Instant::now() < deadline {
            app.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        app.ask.clone().expect("движок должен задать вопрос")
    }

    /// Ответ на панель запроса должен быть виден в буфере сразу — ещё до
    /// следующего вопроса и до конца прогона. Иначе кажется, что кнопка «Ё»
    /// ничего не делает, а замены «е» → «ё» появляются только после «Стоп».
    #[test]
    fn answer_is_shown_before_the_next_question() {
        let mut app = app();
        app.text = "Он берет копье.".to_owned();
        app.text_chars = app.text.chars().count();
        app.start();

        let first = wait_for_ask(&mut app);
        assert_eq!(first.word1, "берет");
        assert_eq!(first.word2, "берёт");
        app.answer(AskDecision::Yo);

        let second = wait_for_ask(&mut app);
        assert_eq!(second.word1, "копье");
        assert!(app.busy(), "второй вопрос должен прийти до конца прогона");
        assert!(
            app.text.starts_with("Он берёт"),
            "замена по ответу должна попасть в буфер сразу, а там: {:?}",
            app.text
        );

        app.answer(AskDecision::Ye);
        wait(&mut app);
        assert_eq!(app.text, "Он берёт копье.");
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

    const ASK_TEXT_TEST: egui::Color32 = egui::Color32::from_rgb(255, 214, 150);
    const ASK_PLATE_TEST: egui::Color32 = egui::Color32::from_rgba_premultiplied(70, 52, 10, 70);

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
                    inner = ask_panel_ui(ui, ask, ASK_TEXT_TEST, ASK_PLATE_TEST);
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
                    row = ask_panel_ui(ui, &a, ASK_TEXT_TEST, ASK_PLATE_TEST).1;
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
                    let (d, _) = ask_panel_ui(ui, &a, ASK_TEXT_TEST, ASK_PLATE_TEST);
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
                    paint_ask_word(ui, &out, range, ASK_PLATE_TEST, 1.5);
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
                paint_ask_word(ui, &out, range, ASK_PLATE_TEST, 1.5);
            });
        });
        let plaques: Vec<Rect> = out
            .shapes
            .iter()
            .filter_map(|cs| match &cs.shape {
                egui::epaint::Shape::Rect(r) if r.fill == ASK_PLATE_TEST => Some(r.rect),
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

// ═══════════════════════════════════════════════════════════════════════════
//  Тесты вспомогательных функций и настроек
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod helper_tests {
    use super::*;

    #[test]
    fn char_and_byte_offsets_agree_on_cyrillic() {
        let text = "Ёж, ёлка и ёрш";
        for (i, (byte, _)) in text.char_indices().enumerate() {
            assert_eq!(char_to_byte(text, i), byte);
            assert_eq!(byte_to_char(text, byte), i);
        }
        assert_eq!(char_to_byte(text, text.chars().count()), text.len());
        assert_eq!(byte_to_char(text, text.len()), text.chars().count());
    }

    #[test]
    fn text_find_walks_forward_and_back_with_wrap() {
        let text = "актер Актер актёр";
        // Вперёд с нуля — первое вхождение.
        assert_eq!(text_find(text, "актер", 0, true, false), Some((0, 5)));
        // Вперёд со второго символа — следующее (регистр не важен).
        assert_eq!(text_find(text, "актер", 1, true, false), Some((6, 11)));
        // Учитывая регистр, «Актер» не совпадает с «актер» — остаётся
        // только зацикленное вхождение в начале строки.
        assert_eq!(text_find(text, "актер", 1, true, true), Some((0, 5)));
        // Назад из конца — последнее вхождение.
        assert_eq!(
            text_find(text, "актер", text.chars().count(), false, false),
            Some((6, 11))
        );
        // Зацикливание вперёд: после конца начинаем с начала.
        assert_eq!(text_find(text, "актер", 7, true, false), Some((0, 5)));
        // Зацикливание назад: раньше нуля продолжаем с конца.
        assert_eq!(text_find(text, "актер", 0, false, false), Some((6, 11)));
        assert_eq!(text_find(text, "", 0, true, false), None);
    }

    #[test]
    fn regex_find_reports_errors_and_matches() {
        let text = "актер Актер актёр";
        let m = regex_find(text, "акт[её]р", 0, true, true).unwrap();
        assert_eq!(m, Some((0, 5)));
        // Назад от конца — «актёр» (последнее совпадение).
        let m = regex_find(text, "акт[её]р", text.chars().count(), false, true).unwrap();
        assert_eq!(m, Some((12, 17)));
        // Кривое выражение — ошибка, а не паника.
        assert!(regex_find(text, "[", 0, true, false).is_err());
    }

    #[test]
    fn system_roots_are_usable_directories() {
        let roots = system_roots();
        assert!(!roots.is_empty(), "должен быть хотя бы один корень");
        for (label, path) in roots {
            assert!(!label.is_empty());
            assert!(path.is_dir(), "корень {label} не каталог");
        }
    }

    #[test]
    fn list_dir_filtered_shows_only_matching_files() {
        let dir = std::env::temp_dir().join(format!("yol-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        std::fs::write(dir.join("b.dat"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();

        let all = list_dir_filtered(&dir, None);
        assert!(all.iter().any(|(n, d)| n == "b.dat" && !d));
        assert!(all.iter().any(|(n, d)| n == "sub" && *d));

        let only_txt = list_dir_filtered(&dir, Some(&["txt"]));
        assert!(only_txt.iter().any(|(n, _)| n == "a.txt"));
        assert!(!only_txt.iter().any(|(n, _)| n == "b.dat"));
        // Папки видны всегда, иначе из каталога не выбраться.
        assert!(only_txt.iter().any(|(n, d)| n == "sub" && *d));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_props_counts_words_and_lines() {
        let text = "Актер вышел.\nБереза стоит.";
        let props = FileProps::collect(None, text, TextEncoding::Utf8Bom);
        assert_eq!(props.chars, text.chars().count());
        assert_eq!(props.lines, 2);
        assert_eq!(props.words, 4);
        assert!(!props.exists);
        assert_eq!(props.size, 0);
    }

    #[test]
    fn format_system_time_renders_the_epoch() {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(0);
        assert_eq!(format_system_time(t), "1970-01-01 00:00");
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_eq!(format_system_time(t), "2023-11-14 22:13");
    }

    #[test]
    fn dictionary_variants_keep_the_case_of_the_text() {
        let dict = yol::yo_core::dictionary();
        let (ye, yo) = dictionary_variants(dict, "актер").expect("слово есть в базе");
        assert_eq!(ye, "актер");
        assert_eq!(yo, "актёр");
        let (ye, yo) = dictionary_variants(dict, "Актер").expect("слово есть в базе");
        assert_eq!(ye, "Актер");
        assert_eq!(yo, "Актёр");
        assert!(dictionary_variants(dict, "мама").is_none());
    }

    #[test]
    fn gui_defaults_ask_about_ambiguous_words() {
        let app_ctx = egui::Context::default();
        let app = YoApp::with_context(&app_ctx);
        assert_eq!(app.opts.ambiguous, AmbiguousPolicy::Ask);
        assert!(app.opts.interactive(), "панель запроса должна быть активна");
        assert!(app.config_path.is_none(), "тесты не пишут yo.ini");
        assert_eq!(app.font_spec.size, FontSpec::default().size);
    }

    #[test]
    fn settings_snapshot_survives_an_ini_round_trip() {
        let ctx = egui::Context::default();
        let mut app = YoApp::with_context(&ctx);
        app.opts.no_var_only = true;
        app.opts.var_only = false;
        app.dark = false;
        app.highlight = false;
        app.mark_text = Rgb::new(1, 2, 3);
        app.mark_bg = Rgb::new(4, 5, 6);
        app.ask_underline = 2.5;
        app.font_spec.size = 21.0;
        app.font_spec.style = FontStyle::Italic;
        app.recent = vec![PathBuf::from("/tmp/один.txt")];

        let snapshot = app.settings_snapshot();
        let parsed = GuiSettings::parse(&snapshot.to_ini());
        assert!(parsed.opts.no_var_only);
        assert!(!parsed.opts.var_only);
        assert!(!parsed.dark);
        assert!(!parsed.highlight);
        assert_eq!(parsed.mark_text, Rgb::new(1, 2, 3));
        assert_eq!(parsed.mark_bg, Rgb::new(4, 5, 6));
        assert_eq!(parsed.ask_underline, 2.5);
        assert_eq!(parsed.font.size, 21.0);
        assert_eq!(parsed.font.style, FontStyle::Italic);
        assert_eq!(parsed.recent, vec![PathBuf::from("/tmp/один.txt")]);
    }

    #[test]
    fn both_exclusive_modes_never_stay_on_together() {
        let mut o = YoOptions::default();
        o.set_no_var_only(true);
        o.set_var_only(true);
        assert!(
            !o.no_var_only,
            "включение «интерактивных» снимает «безопасные»"
        );
        o.set_no_var_only(true);
        assert!(!o.var_only);

        // Так же после чтения испорченного yo.ini.
        let mut parsed = GuiSettings::parse("[Yo]\nNoVarOnly=1\nVarOnly=1\n");
        assert!(parsed.opts.var_only && !parsed.opts.no_var_only);
        parsed.opts.normalize();
        assert!(parsed.opts.var_only && !parsed.opts.no_var_only);
    }

    #[test]
    fn settings_file_is_written_next_to_the_given_path() {
        let dir = std::env::temp_dir().join(format!("yol-ini-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("yo.ini");
        let mut s = GuiSettings::default();
        s.opts.always_ask = true;
        s.recent = vec![PathBuf::from("/tmp/книга.fb2")];
        s.save(&path).expect("файл должен записаться");
        let read = GuiSettings::load(&path);
        assert!(read.opts.always_ask);
        assert_eq!(read.recent, vec![PathBuf::from("/tmp/книга.fb2")]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod cli_mode_tests {
    use super::*;

    fn args_with(input: Option<&str>, output: Option<&str>) -> Args {
        let mut a = Args {
            input: input.map(|s| PathBuf::from(s)),
            output: output.map(|s| PathBuf::from(s)),
            in_enc: None,
            out_enc: None,
            gui: false,
            quiet: false,
            opts: YoOptions::default(),
            help: false,
            version: false,
            error: None,
        };
        a.opts.ambiguous = AmbiguousPolicy::Ask;
        a
    }

    #[test]
    fn double_click_opens_window_not_cli() {
        let a = args_with(None, None);
        assert!(
            !a.is_cli_with(StdinKind::Other),
            "двойной клик должен открывать окно, а не консольный режим"
        );
        assert!(
            !a.is_cli_with(StdinKind::Terminal),
            "терминал без аргументов — тоже окно"
        );
    }

    #[test]
    fn pipe_triggers_cli() {
        let a = args_with(None, None);
        assert!(
            a.is_cli_with(StdinKind::Pipe),
            "cat file | yol должен идти в консольный режим"
        );
    }

    #[test]
    fn file_redirect_triggers_cli() {
        let a = args_with(None, None);
        assert!(
            a.is_cli_with(StdinKind::File),
            "yol < file должен идти в консольный режим"
        );
    }

    #[test]
    fn explicit_input_forces_cli_even_on_terminal() {
        let a = args_with(Some("in.txt"), None);
        assert!(
            a.is_cli_with(StdinKind::Terminal),
            "-i должен форсировать CLI даже в терминале"
        );
        assert!(
            a.is_cli_with(StdinKind::Other),
            "-i должен форсировать CLI даже при Other"
        );
    }

    #[test]
    fn explicit_output_forces_cli() {
        let a = args_with(None, Some("out.txt"));
        assert!(a.is_cli_with(StdinKind::Terminal));
        assert!(a.is_cli_with(StdinKind::Other));
    }

    #[test]
    fn dash_means_explicit_stdio() {
        let a = args_with(Some("-"), None);
        assert!(a.is_cli_with(StdinKind::Terminal));
        let a = Args::parse(["yol".to_string(), "-".to_string()].into_iter());
        assert!(a.input.is_some());
        assert!(a.is_cli_with(StdinKind::Terminal));
    }

    #[test]
    fn help_and_version_never_cli() {
        let mut a = args_with(Some("in.txt"), None);
        a.help = true;
        assert!(!a.is_cli_with(StdinKind::Pipe), "--help не CLI");
        a.help = false;
        a.version = true;
        assert!(!a.is_cli_with(StdinKind::Pipe), "--version не CLI");
    }

    #[test]
    fn gui_flag_forces_window_even_with_pipe() {
        let mut a = args_with(None, None);
        a.gui = true;
        assert!(
            !a.is_cli_with(StdinKind::Pipe),
            "-g должен открывать окно даже с трубой"
        );
        assert!(!a.is_cli_with(StdinKind::File));
    }

    #[test]
    fn stdin_kind_does_not_panic() {
        let k = stdin_kind();
        assert!(matches!(
            k,
            StdinKind::Terminal | StdinKind::Pipe | StdinKind::File | StdinKind::Other
        ));
    }
}
