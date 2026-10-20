//! Интеграционные тесты кодеков.
//!
//! Загрузка и сохранение файлов — единственное место, где программа
//! соприкасается с внешним миром, поэтому ошибка здесь превращает текст
//! в кракозябры. Проверяем распознавание по BOM, фолбэк на CP1251 и
//! симметричность `decode` / `encode`.

use yol::yo_core::{decode_bytes, decode_cp1251, encode_cp1251, encode_string, TextEncoding};

const TEXT: &str = "Актер вышел на сцену, береза у елки. 42.";

#[test]
fn utf8_with_bom_is_recognised() {
    let bytes = encode_string(TEXT, TextEncoding::Utf8Bom);
    let (text, enc) = decode_bytes(&bytes);
    assert_eq!(enc, TextEncoding::Utf8Bom);
    assert_eq!(text, TEXT);
}

#[test]
fn utf8_without_bom_is_recognised() {
    let bytes = encode_string(TEXT, TextEncoding::Utf8);
    let (text, enc) = decode_bytes(&bytes);
    assert_eq!(enc, TextEncoding::Utf8);
    assert_eq!(text, TEXT);
}

#[test]
fn utf16_le_with_bom_is_recognised() {
    let bytes = encode_string(TEXT, TextEncoding::Utf16Le);
    let (text, enc) = decode_bytes(&bytes);
    assert_eq!(enc, TextEncoding::Utf16Le);
    assert_eq!(text, TEXT);
}

#[test]
fn utf16_be_with_bom_is_recognised() {
    let bytes = encode_string(TEXT, TextEncoding::Utf16Be);
    let (text, enc) = decode_bytes(&bytes);
    assert_eq!(enc, TextEncoding::Utf16Be);
    assert_eq!(text, TEXT);
}

#[test]
fn non_utf8_without_bom_falls_back_to_cp1251() {
    let bytes = encode_cp1251(TEXT);
    // sanity: это действительно не валидный UTF-8
    assert!(std::str::from_utf8(&bytes).is_err());

    let (text, enc) = decode_bytes(&bytes);
    assert_eq!(enc, TextEncoding::Cp1251);
    assert_eq!(text, TEXT);
}

#[test]
fn every_encoding_roundtrips() {
    for enc in TextEncoding::ALL {
        let bytes = encode_string(TEXT, enc);
        let (text, detected) = decode_bytes(&bytes);
        assert_eq!(text, TEXT, "круговой проход для {enc:?}");
        assert_eq!(detected, enc, "распознавание для {enc:?}");
    }
}

#[test]
fn cp1251_table_matches_windows() {
    // Контрольные точки: Ё/ё и границы диапазона А–Я / а–я.
    assert_eq!(decode_cp1251(&[0xA8]), "Ё");
    assert_eq!(decode_cp1251(&[0xB8]), "ё");
    assert_eq!(decode_cp1251(&[0xC0]), "А");
    assert_eq!(decode_cp1251(&[0xDF]), "Я");
    assert_eq!(decode_cp1251(&[0xE0]), "а");
    assert_eq!(decode_cp1251(&[0xFF]), "я");

    // Обратно.
    assert_eq!(encode_cp1251("Ё"), vec![0xA8]);
    assert_eq!(encode_cp1251("ё"), vec![0xB8]);
    assert_eq!(encode_cp1251("Ая"), vec![0xC0, 0xFF]);
}

#[test]
fn ascii_is_ascii_in_cp1251() {
    let src = "Hello, world! 123\n\t";
    assert_eq!(encode_cp1251(src), src.as_bytes().to_vec());
    assert_eq!(decode_cp1251(src.as_bytes()), src);
}

#[test]
fn cp1251_roundtrip_is_lossless_on_cyrillic() {
    for src in [
        "ёлка елка ЁЛКА ЕЛКА",
        "Актер, берёза; орёл — мёд!",
        "№1 — 42% (тест)",
    ] {
        assert_eq!(decode_cp1251(&encode_cp1251(src)), src, "для {src:?}");
    }
}

/// Пустой файл не должен ничего ломать ни в одном формате.
#[test]
fn empty_input_decodes_to_empty_string() {
    for enc in TextEncoding::ALL {
        let bytes = encode_string("", enc);
        let (text, _) = decode_bytes(&bytes);
        assert!(text.is_empty(), "для {enc:?}");
    }
    assert_eq!(decode_cp1251(&[]), "");
}
