//! # YOL — «Ё лайт»
//!
//! Ядро ёфикации: разбор встроенных баз, конечный автомат `SelectWord`,
//! словарь регулярных выражений, кодеки и потоковый движок.
//!
//! Библиотека существует отдельно от `main.rs` по двум причинам:
//!
//! 1. интеграционные тесты из [`tests/`](../../tests) могут пользоваться
//!    публичным API (`use yol::yo_core::…`); у бинарного крейта так нельзя;
//! 2. ядро можно подключить в другое приложение, не таща за собой egui.
//!
//! ## Устройство
//!
//! ```text
//!   yox.dat ──► Dictionary::parse   ──┐
//!                                     ├─► Engine::process(input, &mut sink)
//!   yo_regexpr.dat ─► RegexDict::parse ┘        │
//!                                              └─► Sink::chunk / stats / ask
//! ```
//!
//! Обе базы встроены в бинарник через `include_bytes!` — обращений к диску
//! во время работы нет.
//!
//! ## Пример
//!
//! ```
//! use yol::yo_core::{Engine, NullSink, YoOptions};
//!
//! let mut engine = Engine::new(YoOptions::default());
//! let stats = engine.process("Актер вышел на сцену.", &mut NullSink);
//! assert!(stats.checked_changed >= 1);
//! ```

// В ядре нет ни одного `unsafe`-блока; атрибут закрепляет это на будущее.
#![deny(unsafe_code)]

pub mod yo_core;

// Порядок внутри списка как у rustfmt: сначала snake_case-функции,
// потом типы в CamelCase. `YO_VERSION` намеренно не реэкспортируется:
// сортировка `YO_VERSION` рядом с `YoOptions` неоднозначна, поэтому
// константу берут по полному пути — `yol::yo_core::YO_VERSION`.
pub use yo_core::{
    clear_user_selections, copy_case, cp1251_char, database_info, decode_bytes, decode_cp1251,
    dictionary, encode_cp1251, encode_string, is_abbreviation_at, is_alpha, is_blank,
    is_ellipsis_at, is_lower, is_upper, is_ye, is_ye_yo, is_ye_yo_in, is_yo, is_yo_in, lower_char,
    lower_into, make_valid_regexpr, match_regexp, preload, regex_dictionary, to_ye, to_ye_into,
    upper_char, AmbiguousPolicy, AskDecision, AskRequest, Dictionary, Engine, NullSink, RegexDict,
    RegexRule, Sink, Stats, TextEncoding, WriterSink, YoOptions,
};
