//! Пользовательские настройки и файл `yo.ini`.
//!
//! Delphi-оригинал хранил всё в `yo.ini` рядом с программой
//! (`TMainForm.ReadIni` / `WriteIni`): флажки вкладки «Ёфикация», размер
//! и имя шрифта редактора, цвета пометок и список последних файлов.
//! Порт сохраняет ту же схему — файл остаётся читаемым и «по-человечески»
//! редактируемым, а ключи по возможности совпадают с оригинальными.
//!
//! Модуль намеренно не знает про `egui`: здесь только данные и разбор INI.
//! Цвета хранятся как обычные компоненты RGB/RGBA, а `main.rs` переводит их
//! в [`egui::Color32`].
//!
//! # Совместимость
//!
//! * `[Yo]` — те же ключи, что в Delphi-версии (`AlwaysAsk`, `Mark`,
//!   `CheckYo`, `NoVarOnly`, `VarOnly`, `WordWrap`, `ProposeLast`, `FBFormat`,
//!   `AutoUnicode`, `RegExprs`, `RecallLastFile`, `ToConfirmAbbr`,
//!   `ToConfirmEllipsis`, `ToConfirmCap`, `LinesBelow`, `Dictionary`,
//!   `RegExprDictionary`);
//! * `[Colors]` — `ColorMark`/`ColorBackMark` в формате `TColor`
//!   (`$BBGGRR` или десятичное число), плюс новые ключи оформления;
//! * `[Files]` — `Name1`…`NameN` и `LastFile`, как в оригинале.
//!
//! Файл читается через [`crate::yo_core::decode_bytes`], поэтому подходят
//! UTF-8 (с BOM и без), UTF-16 и ANSI/CP1251 — старый `yo.ini` открывается
//! без конвертации.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::yo_core::{decode_bytes, AmbiguousPolicy, YoOptions};

/// Имя файла настроек (в каталоге программы, как в оригинале).
pub const INI_NAME: &str = "yo.ini";

/// Сколько последних файлов помнить. В Delphi — `MaxFileNamesInList`.
pub const MAX_RECENT: usize = 10;

// ═══════════════════════════════════════════════════════════════════════════
//  Цвета
// ═══════════════════════════════════════════════════════════════════════════

/// Цвет без прозрачности.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Разбор `#RRGGBB`, `$BBGGRR` (как `TColor` в Delphi) или десятичного
    /// числа — тоже `TColor`.
    pub fn parse(text: &str) -> Option<Self> {
        let t = text.trim();
        if t.is_empty() {
            return None;
        }
        if let Some(hex) = t.strip_prefix('#').or_else(|| t.strip_prefix("0x")) {
            let v = u32::from_str_radix(hex, 16).ok()?;
            return Some(Self::from_rgb_hex(v));
        }
        if let Some(hex) = t.strip_prefix('$') {
            let v = u32::from_str_radix(hex, 16).ok()?;
            return Some(Self::from_tcolor(v));
        }
        if let Ok(dec) = t.parse::<u32>() {
            // Так `TIniFile.WriteInteger` писал `clBlue` и прочие константы.
            return Some(Self::from_tcolor(dec));
        }
        // Шесть hex-цифр без префикса — считаем RRGGBB.
        if t.len() == 6 {
            let v = u32::from_str_radix(t, 16).ok()?;
            return Some(Self::from_rgb_hex(v));
        }
        None
    }

    fn from_rgb_hex(v: u32) -> Self {
        Self {
            r: ((v >> 16) & 0xFF) as u8,
            g: ((v >> 8) & 0xFF) as u8,
            b: (v & 0xFF) as u8,
        }
    }

    /// `TColor` хранит цвет как `0x00BBGGRR`.
    fn from_tcolor(v: u32) -> Self {
        Self {
            r: (v & 0xFF) as u8,
            g: ((v >> 8) & 0xFF) as u8,
            b: ((v >> 16) & 0xFF) as u8,
        }
    }

    pub fn to_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Цвет с прозрачностью (0 — полностью прозрачный, 255 — непрозрачный).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// `#RRGGBBAA`, `#RRGGBB` (тогда `a = 255`) либо `TColor`.
    pub fn parse(text: &str) -> Option<Self> {
        let t = text.trim();
        if let Some(hex) = t.strip_prefix('#').or_else(|| t.strip_prefix("0x")) {
            let digits = hex.trim();
            let v = u32::from_str_radix(digits, 16).ok()?;
            return Some(if digits.len() > 6 {
                Self {
                    r: ((v >> 24) & 0xFF) as u8,
                    g: ((v >> 16) & 0xFF) as u8,
                    b: ((v >> 8) & 0xFF) as u8,
                    a: (v & 0xFF) as u8,
                }
            } else {
                let c = Rgb::from_rgb_hex(v);
                Self::new(c.r, c.g, c.b, 255)
            });
        }
        let c = Rgb::parse(t)?;
        Some(Self::new(c.r, c.g, c.b, 255))
    }

    pub fn rgb(self) -> Rgb {
        Rgb::new(self.r, self.g, self.b)
    }

    pub fn to_hex(self) -> String {
        if self.a == 255 {
            format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
        } else {
            format!("#{:02X}{:02X}{:02X}{:02X}", self.r, self.g, self.b, self.a)
        }
    }
}

impl fmt::Display for Rgba {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Шрифт редактора
// ═══════════════════════════════════════════════════════════════════════════

/// Начертание шрифта. Для встроенных шрифтов курсив синтезируется epaint'ом,
/// а «полужирный» берётся из отдельного файла семейства, если он есть.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FontStyle {
    /// Обычное начертание.
    #[default]
    Regular,
    /// Курсив.
    Italic,
    /// Полужирный.
    Bold,
    /// Полужирный курсив.
    BoldItalic,
}

impl FontStyle {
    pub const ALL: [FontStyle; 4] = [
        FontStyle::Regular,
        FontStyle::Italic,
        FontStyle::Bold,
        FontStyle::BoldItalic,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FontStyle::Regular => "Обычное",
            FontStyle::Italic => "Курсив",
            FontStyle::Bold => "Полужирный",
            FontStyle::BoldItalic => "Полужирный курсив",
        }
    }

    /// Строчное имя для `yo.ini`.
    pub fn key(self) -> &'static str {
        match self {
            FontStyle::Regular => "regular",
            FontStyle::Italic => "italic",
            FontStyle::Bold => "bold",
            FontStyle::BoldItalic => "bolditalic",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "regular" | "normal" | "обычное" => Some(FontStyle::Regular),
            "italic" | "курсив" => Some(FontStyle::Italic),
            "bold" | "полужирный" => Some(FontStyle::Bold),
            "bolditalic" | "bold-italic" | "полужирный курсив" => {
                Some(FontStyle::BoldItalic)
            }
            _ => None,
        }
    }

    pub fn is_bold(self) -> bool {
        matches!(self, FontStyle::Bold | FontStyle::BoldItalic)
    }

    pub fn is_italic(self) -> bool {
        matches!(self, FontStyle::Italic | FontStyle::BoldItalic)
    }

    /// Начертание без курсива — для подбора файла, если курсивного нет.
    pub fn without_italic(self) -> Self {
        match self {
            FontStyle::Italic => FontStyle::Regular,
            FontStyle::BoldItalic => FontStyle::Bold,
            other => other,
        }
    }

    /// Начертание без жирности — для подбора файла, если жирного нет.
    pub fn without_bold(self) -> Self {
        match self {
            FontStyle::Bold => FontStyle::Regular,
            FontStyle::BoldItalic => FontStyle::Italic,
            other => other,
        }
    }
}

impl fmt::Display for FontStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Описание шрифта редактора: семейство, конкретный файл (если он не
/// встроенный), начертание и размер.
#[derive(Clone, Debug, PartialEq)]
pub struct FontSpec {
    /// Имя семейства для интерфейса. Для встроенных — «Hack» или «Ubuntu Light».
    pub family: String,
    /// Файл `.ttf`/`.otf`, из которого загружен шрифт; `None` — встроенный.
    pub file: Option<PathBuf>,
    pub style: FontStyle,
    pub size: f32,
}

impl Default for FontSpec {
    fn default() -> Self {
        Self {
            // Моноширинный, как `Courier New` оригинала.
            family: "Hack".to_owned(),
            file: None,
            style: FontStyle::Regular,
            size: 14.5,
        }
    }
}

/// Допустимый диапазон кегля.
pub const MIN_FONT_SIZE: f32 = 8.0;
pub const MAX_FONT_SIZE: f32 = 40.0;

impl FontSpec {
    /// Привести значения в разумные рамки (после чтения `yo.ini`).
    pub fn sanitized(mut self) -> Self {
        if !self.size.is_finite() {
            self.size = FontSpec::default().size;
        }
        self.size = self.size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        if self.family.trim().is_empty() {
            self.family = FontSpec::default().family;
        }
        self
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Настройки приложения
// ═══════════════════════════════════════════════════════════════════════════

/// Полный набор настроек: режимы ёфикации, вид редактора и оформление.
#[derive(Clone, Debug, PartialEq)]
pub struct GuiSettings {
    pub opts: YoOptions,

    // ── вид ────────────────────────────────────────────────────────────────
    pub dark: bool,
    /// Подсвечивать заменённые буквы в редакторе.
    pub highlight: bool,
    /// Показывать окно журнала замен.
    pub show_journal: bool,

    // ── типографика ────────────────────────────────────────────────────────
    pub font: FontSpec,

    // ── подсветка ──────────────────────────────────────────────────────────
    /// Цвет изменённых букв.
    pub mark_text: Rgb,
    /// Плашка под изменёнными буквами.
    pub mark_bg: Rgb,
    /// Цвет текста спорного слова (панель запроса).
    pub ask_text: Rgb,
    /// Плашка под спорным словом.
    pub ask_plate: Rgba,
    /// Толщина подчёркивания спорного слова.
    pub ask_underline: f32,

    // ── файлы ──────────────────────────────────────────────────────────────
    /// Последние открытые файлы, свежие первыми.
    pub recent: Vec<PathBuf>,
    /// Последний открытый файл (для `RecallLastFile`).
    pub last_file: Option<PathBuf>,
    /// Внешний основной словарь, если пользователь подгрузил свой.
    pub dictionary: Option<PathBuf>,
    /// Внешний словарь регулярных выражений.
    pub regex_dictionary: Option<PathBuf>,
}

impl Default for GuiSettings {
    fn default() -> Self {
        // В интерактивном приложении неоднозначные слова по умолчанию
        // спрашивают — ровно как требует панель запроса оригинала.
        let opts = YoOptions {
            ambiguous: AmbiguousPolicy::Ask,
            ..Default::default()
        };
        Self {
            opts,
            dark: true,
            highlight: true,
            show_journal: false,
            font: FontSpec::default(),
            mark_text: Rgb::new(255, 208, 96),
            mark_bg: Rgb::new(96, 62, 12),
            ask_text: Rgb::new(255, 214, 150),
            ask_plate: Rgba::new(70, 52, 10, 90),
            ask_underline: 1.5,
            recent: Vec::new(),
            last_file: None,
            dictionary: None,
            regex_dictionary: None,
        }
    }
}

impl GuiSettings {
    /// Добавить файл в список последних (без дублей, свежий — первым).
    pub fn remember_file(&mut self, path: &Path) {
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_path_buf());
        self.recent.truncate(MAX_RECENT);
        self.last_file = Some(path.to_path_buf());
    }

    /// Убрать файл из списка последних.
    pub fn forget_file(&mut self, path: &Path) {
        self.recent.retain(|p| p != path);
        if self.last_file.as_deref() == Some(path) {
            self.last_file = None;
        }
    }

    /// Полный разбор `yo.ini`. Неизвестные ключи молча игнорируются,
    /// отсутствующие — берут значения по умолчанию.
    pub fn parse(text: &str) -> Self {
        let ini = Ini::parse(text);
        let mut s = Self::default();

        // ── [Yo] ───────────────────────────────────────────────────────────
        s.opts.always_ask = ini.bool_in("Yo", "AlwaysAsk", s.opts.always_ask);
        s.opts.mark = ini.bool_in("Yo", "Mark", s.opts.mark);
        s.opts.fast_scroll = ini.bool_in("Yo", "FastScroll", s.opts.fast_scroll);
        s.opts.check_yo = ini.bool_in("Yo", "CheckYo", s.opts.check_yo);
        s.opts.no_var_only = ini.bool_in("Yo", "NoVarOnly", s.opts.no_var_only);
        s.opts.var_only = ini.bool_in("Yo", "VarOnly", s.opts.var_only);
        s.opts.word_wrap = ini.bool_in("Yo", "WordWrap", s.opts.word_wrap);
        s.opts.propose_last = ini.bool_in("Yo", "ProposeLast", s.opts.propose_last);
        s.opts.fb_format = ini.bool_in("Yo", "FBFormat", s.opts.fb_format);
        s.opts.auto_unicode = ini.bool_in("Yo", "AutoUnicode", s.opts.auto_unicode);
        s.opts.reg_exprs = ini.bool_in("Yo", "RegExprs", s.opts.reg_exprs);
        s.opts.last_file = ini.bool_in("Yo", "RecallLastFile", s.opts.last_file);
        s.opts.to_confirm_abbr = ini.bool_in("Yo", "ToConfirmAbbr", s.opts.to_confirm_abbr);
        s.opts.to_confirm_cap = ini.bool_in("Yo", "ToConfirmCap", s.opts.to_confirm_cap);
        s.opts.to_confirm_ellipsis =
            ini.bool_in("Yo", "ToConfirmEllipsis", s.opts.to_confirm_ellipsis);
        s.opts.lines_below = ini
            .int_in("Yo", "LinesBelow", s.opts.lines_below as i64)
            .clamp(0, 100) as usize;

        let policy = ini.get("Yo", "Ambiguous").unwrap_or_default();
        s.opts.ambiguous = AmbiguousPolicy::parse(&policy).unwrap_or(s.opts.ambiguous);
        // Инвариант: «только безопасные» и «только интерактивные» несовместимы.
        s.opts.normalize();

        // ── [View] ─────────────────────────────────────────────────────────
        s.dark = ini.bool_in("View", "Dark", s.dark);
        s.highlight = ini.bool_in("View", "Highlight", s.highlight);
        s.show_journal = ini.bool_in("View", "Journal", s.show_journal);

        // ── [Colors] ───────────────────────────────────────────────────────
        if let Some(c) = ini.get("Colors", "ColorMark").and_then(|v| Rgb::parse(&v)) {
            s.mark_text = c;
        }
        if let Some(c) = ini
            .get("Colors", "ColorBackMark")
            .and_then(|v| Rgb::parse(&v))
        {
            s.mark_bg = c;
        }
        if let Some(c) = ini
            .get("Colors", "AskTextColor")
            .and_then(|v| Rgb::parse(&v))
        {
            s.ask_text = c;
        }
        if let Some(c) = ini
            .get("Colors", "AskBackColor")
            .and_then(|v| Rgba::parse(&v))
        {
            s.ask_plate = c;
        }
        if let Some(w) = ini
            .get("Colors", "AskUnderline")
            .and_then(|v| parse_f32(&v))
        {
            s.ask_underline = w.clamp(0.0, 10.0);
        }

        // ── [Font] ─────────────────────────────────────────────────────────
        let fb = &mut s.font;
        fb.family = ini
            .get("Font", "Name")
            .or_else(|| ini.get("Yo", "FontName"))
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| fb.family.clone());
        fb.size = ini
            .get("Font", "Size")
            .or_else(|| ini.get("Yo", "FontSize"))
            .and_then(|v| parse_f32(&v))
            .unwrap_or(fb.size);
        fb.style = ini
            .get("Font", "Style")
            .or_else(|| ini.get("Yo", "FontStyle"))
            .and_then(|v| FontStyle::parse(&v))
            .unwrap_or(fb.style);
        fb.file = ini
            .get("Font", "File")
            .map(|v| PathBuf::from(v.trim()))
            .filter(|p| !p.as_os_str().is_empty());
        s.font = fb.clone().sanitized();

        // ── [Files] ────────────────────────────────────────────────────────
        for i in 1..=MAX_RECENT {
            if let Some(name) = ini.get("Files", &format!("Name{i}")) {
                let name = name.trim();
                if !name.is_empty() {
                    let path = PathBuf::from(name);
                    if !s.recent.contains(&path) {
                        s.recent.push(path);
                    }
                }
            }
        }
        s.recent.truncate(MAX_RECENT);
        s.last_file = ini
            .get("Files", "LastFile")
            .map(|v| PathBuf::from(v.trim()))
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| s.recent.first().cloned());
        s.dictionary = ini
            .get("Yo", "Dictionary")
            .map(|v| PathBuf::from(v.trim()))
            .filter(|p| !p.as_os_str().is_empty());
        s.regex_dictionary = ini
            .get("Yo", "RegExprDictionary")
            .map(|v| PathBuf::from(v.trim()))
            .filter(|p| !p.as_os_str().is_empty());
        s
    }

    /// Сериализовать в тот же `yo.ini`.
    pub fn to_ini(&self) -> String {
        let mut w = IniWriter::default();
        let o = &self.opts;
        w.bool("Yo", "AlwaysAsk", o.always_ask);
        w.bool("Yo", "Mark", o.mark);
        w.bool("Yo", "FastScroll", o.fast_scroll);
        w.bool("Yo", "CheckYo", o.check_yo);
        w.bool("Yo", "NoVarOnly", o.no_var_only);
        w.bool("Yo", "VarOnly", o.var_only);
        w.bool("Yo", "WordWrap", o.word_wrap);
        w.bool("Yo", "ProposeLast", o.propose_last);
        w.bool("Yo", "FBFormat", o.fb_format);
        w.bool("Yo", "AutoUnicode", o.auto_unicode);
        w.bool("Yo", "RegExprs", o.reg_exprs);
        w.bool("Yo", "RecallLastFile", o.last_file);
        w.bool("Yo", "ToConfirmAbbr", o.to_confirm_abbr);
        w.bool("Yo", "ToConfirmCap", o.to_confirm_cap);
        w.bool("Yo", "ToConfirmEllipsis", o.to_confirm_ellipsis);
        w.raw("Yo", "LinesBelow", &o.lines_below.to_string());
        w.raw("Yo", "Ambiguous", o.ambiguous.key());
        if let Some(p) = &self.dictionary {
            w.raw("Yo", "Dictionary", &p.to_string_lossy());
        }
        if let Some(p) = &self.regex_dictionary {
            w.raw("Yo", "RegExprDictionary", &p.to_string_lossy());
        }

        w.section("View");
        w.bool("View", "Dark", self.dark);
        w.bool("View", "Highlight", self.highlight);
        w.bool("View", "Journal", self.show_journal);

        w.section("Font");
        w.raw("Font", "Name", &self.font.family);
        w.raw("Font", "Size", &format!("{:.1}", self.font.size));
        w.raw("Font", "Style", self.font.style.key());
        if let Some(p) = &self.font.file {
            w.raw("Font", "File", &p.to_string_lossy());
        }

        w.section("Colors");
        w.raw("Colors", "ColorMark", &self.mark_text.to_hex());
        w.raw("Colors", "ColorBackMark", &self.mark_bg.to_hex());
        w.raw("Colors", "AskTextColor", &self.ask_text.to_hex());
        w.raw("Colors", "AskBackColor", &self.ask_plate.to_hex());
        w.raw(
            "Colors",
            "AskUnderline",
            &format!("{:.2}", self.ask_underline),
        );

        w.section("Files");
        for (i, p) in self.recent.iter().take(MAX_RECENT).enumerate() {
            w.raw("Files", &format!("Name{}", i + 1), &p.to_string_lossy());
        }
        if let Some(p) = &self.last_file {
            w.raw("Files", "LastFile", &p.to_string_lossy());
        }
        w.finish()
    }

    /// Прочитать настройки из файла; при любой ошибке — значения по умолчанию.
    pub fn load(path: &Path) -> Self {
        match fs::read(path) {
            Ok(bytes) => {
                let (text, _) = decode_bytes(&bytes);
                Self::parse(&text)
            }
            Err(_) => Self::default(),
        }
    }

    /// Записать настройки. Ошибку возвращаем, чтобы интерфейс мог показать
    /// её в строке состояния.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = fs::create_dir_all(dir);
            }
        }
        fs::write(path, self.to_ini().into_bytes())
    }
}

/// Путь к `yo.ini`: рядом с исполняемым файлом, как в Delphi-версии.
///
/// Переменная окружения `YOL_INI` позволяет указать другой файл — это удобно
/// для портативных сборок и для тестов.
pub fn default_ini_path() -> PathBuf {
    if let Some(p) = std::env::var_os("YOL_INI") {
        return PathBuf::from(p);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(INI_NAME)))
        .unwrap_or_else(|| PathBuf::from(INI_NAME))
}

fn parse_f32(s: &str) -> Option<f32> {
    // В русской локали Delphi мог записать «14,5».
    s.trim().replace(',', ".").parse::<f32>().ok()
}

// ═══════════════════════════════════════════════════════════════════════════
//  Минимальный INI-парсер (без внешних зависимостей)
// ═══════════════════════════════════════════════════════════════════════════

/// Разобранный INI: значения ищутся без учёта регистра ключей и секций.
#[derive(Debug, Default)]
pub struct Ini {
    entries: Vec<(String, String, String)>,
}

impl Ini {
    pub fn parse(text: &str) -> Self {
        let mut entries = Vec::new();
        let mut section = String::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.trim().to_lowercase();
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            entries.push((
                section.clone(),
                key.trim().to_lowercase(),
                value.trim().to_string(),
            ));
        }
        Self { entries }
    }

    /// Значение ключа; при повторе побеждает последнее (как в `TIniFile`).
    pub fn get(&self, section: &str, key: &str) -> Option<String> {
        let section = section.to_lowercase();
        let key = key.to_lowercase();
        self.entries
            .iter()
            .rev()
            .find(|(s, k, _)| *s == section && *k == key)
            .map(|(_, _, v)| v.clone())
    }

    fn bool_in(&self, section: &str, key: &str, default: bool) -> bool {
        self.get(section, key)
            .and_then(|v| parse_bool(&v))
            .unwrap_or(default)
    }

    fn int_in(&self, section: &str, key: &str, default: i64) -> i64 {
        self.get(section, key)
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(default)
    }
}

/// `TIniFile.ReadBool` понимает `0/1`, а люди пишут `true/false`, `да/нет`.
pub fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "да" => Some(true),
        "0" | "false" | "no" | "off" | "нет" => Some(false),
        _ => None,
    }
}

/// Писатель INI: секции и ключи в стабильном порядке.
#[derive(Default)]
struct IniWriter {
    out: String,
    section: String,
}

impl IniWriter {
    fn section(&mut self, name: &str) {
        if !self.out.is_empty() {
            self.out.push('\n');
        }
        self.section = name.to_owned();
        self.out.push('[');
        self.out.push_str(name);
        self.out.push_str("]\n");
    }

    fn raw(&mut self, section: &str, key: &str, value: &str) {
        if self.section != section {
            self.section(section);
        }
        self.out.push_str(key);
        self.out.push('=');
        self.out.push_str(value);
        self.out.push('\n');
    }

    fn bool(&mut self, section: &str, key: &str, value: bool) {
        self.raw(section, key, if value { "1" } else { "0" });
    }

    fn finish(self) -> String {
        self.out
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Тесты
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_ask_about_ambiguous_words() {
        let s = GuiSettings::default();
        assert_eq!(s.opts.ambiguous, AmbiguousPolicy::Ask);
        assert!(s.opts.reg_exprs);
        assert_eq!(s.font.size, 14.5);
        assert_eq!(s.font.style, FontStyle::Regular);
    }

    #[test]
    fn round_trip_keeps_every_setting() {
        let mut s = GuiSettings::default();
        s.opts.mark = false;
        s.opts.word_wrap = true;
        s.opts.lines_below = 7;
        s.opts.ambiguous = AmbiguousPolicy::Ye;
        s.dark = false;
        s.highlight = false;
        s.show_journal = true;
        s.font.family = "DejaVu Sans".to_owned();
        s.font.file = Some(PathBuf::from("/usr/share/fonts/DejaVuSans.ttf"));
        s.font.style = FontStyle::BoldItalic;
        s.font.size = 17.5;
        s.mark_text = Rgb::new(1, 2, 3);
        s.mark_bg = Rgb::new(250, 240, 230);
        s.ask_text = Rgb::new(10, 20, 30);
        s.ask_plate = Rgba::new(40, 50, 60, 70);
        s.ask_underline = 2.5;
        s.recent = vec![PathBuf::from("/tmp/b.txt"), PathBuf::from("/tmp/a.txt")];
        s.last_file = Some(PathBuf::from("/tmp/b.txt"));

        let text = s.to_ini();
        let back = GuiSettings::parse(&text);
        assert_eq!(back, s, "ini:\n{text}");
    }

    #[test]
    fn delphi_style_keys_are_understood() {
        // Именно так писал yo.ini Delphi-оригинал: TColor в десятичном виде
        // и флажки 0/1.
        let ini = "\
[Yo]
AlwaysAsk=1
Mark=0
RegExprs=0
LinesBelow=5
NoVarOnly=1
VarOnly=1
[Colors]
ColorMark=16711680
ColorBackMark=$00FFFF
[Files]
Name1=C:\\texts\\one.txt
Name2=C:\\texts\\two.txt
LastFile=C:\\texts\\two.txt
";
        let s = GuiSettings::parse(ini);
        assert!(s.opts.always_ask);
        assert!(!s.opts.mark);
        assert!(!s.opts.reg_exprs);
        assert_eq!(s.opts.lines_below, 5);
        // Оба флажка стояли в файле — остаться должен только один.
        assert_ne!(s.opts.no_var_only, s.opts.var_only);
        // clBlue = $00FF0000 = BGR → #0000FF.
        assert_eq!(s.mark_text, Rgb::new(0, 0, 255));
        // $00FFFF = BGR → #FFFF00.
        assert_eq!(s.mark_bg, Rgb::new(255, 255, 0));
        assert_eq!(s.recent.len(), 2);
        assert_eq!(
            s.last_file.as_deref(),
            Some(Path::new("C:\\texts\\two.txt"))
        );
    }

    #[test]
    fn mutually_exclusive_modes_are_normalised() {
        let s = GuiSettings::parse("[Yo]\nNoVarOnly=1\nVarOnly=1\n");
        assert!(s.opts.no_var_only ^ s.opts.var_only);
    }

    #[test]
    fn unknown_keys_and_garbage_fall_back_to_defaults() {
        let s = GuiSettings::parse("[Yo]\nLinesBelow=не число\nUnknown=1\n[Broken\n");
        assert_eq!(s.opts.lines_below, YoOptions::default().lines_below);
        assert_eq!(s.opts.ambiguous, AmbiguousPolicy::Ask);
    }

    #[test]
    fn remember_file_keeps_order_and_limit() {
        let mut s = GuiSettings::default();
        for i in 0..(MAX_RECENT + 3) {
            s.remember_file(Path::new(&format!("/tmp/{i}.txt")));
        }
        assert_eq!(s.recent.len(), MAX_RECENT);
        assert_eq!(
            s.recent[0],
            PathBuf::from(format!("/tmp/{}.txt", MAX_RECENT + 2))
        );
        // Повторное открытие поднимает файл наверх, а не дублирует.
        s.remember_file(Path::new("/tmp/5.txt"));
        assert_eq!(s.recent[0], PathBuf::from("/tmp/5.txt"));
        assert_eq!(
            s.recent
                .iter()
                .filter(|p| *p == Path::new("/tmp/5.txt"))
                .count(),
            1
        );
        s.forget_file(Path::new("/tmp/5.txt"));
        assert!(!s.recent.iter().any(|p| p == Path::new("/tmp/5.txt")));
    }

    #[test]
    fn colors_accept_all_documented_forms() {
        assert_eq!(Rgb::parse("#FFD060"), Some(Rgb::new(255, 208, 96)));
        assert_eq!(Rgb::parse("0xFFD060"), Some(Rgb::new(255, 208, 96)));
        assert_eq!(Rgb::parse("FFD060"), Some(Rgb::new(255, 208, 96)));
        assert_eq!(Rgb::parse("$60D0FF"), Some(Rgb::new(255, 208, 96)));
        // clYellow = $00FFFF = 65535 → BGR → #FFFF00
        assert_eq!(Rgb::parse("65535"), Some(Rgb::new(255, 255, 0)));
        assert_eq!(Rgb::parse(""), None);
        assert_eq!(
            Rgba::parse("#11223344"),
            Some(Rgba::new(0x11, 0x22, 0x33, 0x44))
        );
        assert_eq!(
            Rgba::parse("#112233"),
            Some(Rgba::new(0x11, 0x22, 0x33, 255))
        );
    }

    #[test]
    fn font_style_names_round_trip() {
        for style in FontStyle::ALL {
            assert_eq!(FontStyle::parse(style.key()), Some(style));
        }
        assert_eq!(FontStyle::parse("garbage"), None);
        assert!(FontStyle::BoldItalic.is_bold());
        assert!(FontStyle::BoldItalic.is_italic());
        assert_eq!(FontStyle::BoldItalic.without_bold(), FontStyle::Italic);
    }

    #[test]
    fn font_size_is_clamped() {
        let s = GuiSettings::parse("[Font]\nSize=500\n");
        assert_eq!(s.font.size, MAX_FONT_SIZE);
        let s = GuiSettings::parse("[Font]\nSize=1\n");
        assert_eq!(s.font.size, MIN_FONT_SIZE);
        let s = GuiSettings::parse("[Font]\nSize=14,5\n");
        assert_eq!(s.font.size, 14.5);
    }

    #[test]
    fn save_and_load_round_trip_on_disk() {
        let dir = std::env::temp_dir().join(format!("yol-ini-{}", std::process::id()));
        let path = dir.join("yo.ini");
        let mut s = GuiSettings::default();
        s.font.size = 16.0;
        s.opts.ambiguous = AmbiguousPolicy::Yo;
        s.save(&path).expect("запись настроек");
        let back = GuiSettings::load(&path);
        assert_eq!(back, s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_means_defaults() {
        let s = GuiSettings::load(Path::new("/nonexistent/definitely/yo.ini"));
        assert_eq!(s, GuiSettings::default());
    }

    #[test]
    fn cp1251_ini_is_readable() {
        // Старый ANSI-файл: имя шрифта по-русски в CP1251.
        let mut bytes = b"[Font]\nName=".to_vec();
        bytes.push(0xCA);
        bytes.push(0xF3);
        bytes.push(0xF0);
        bytes.push(0xFC);
        bytes.push(0xE5);
        bytes.push(0xF0);
        bytes.push(b'\n');
        let (text, _) = crate::yo_core::decode_bytes(&bytes);
        let s = GuiSettings::parse(&text);
        assert_eq!(s.font.family, "Курьер");
    }
}
