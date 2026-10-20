//! Интеграционные тесты консольного режима.
//!
//! Запускают настоящий собранный бинарник (`CARGO_BIN_EXE_yol`) и проверяют
//! поведение «снаружи»: коды возврата, кодировки, потоковую запись в файл.
//!
//! GUI при этом не запускается: наличие `-i`/`-o` переводит программу в
//! консольный режим, поэтому тесты работают и на безголовых CI-раннерах.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use yol::yo_core::{decode_cp1251, encode_cp1251};

fn yol() -> Command {
    Command::new(env!("CARGO_BIN_EXE_yol"))
}

/// Уникальный путь во временном каталоге: тесты внутри одного бинарника
/// идут параллельно, поэтому имена не должны пересекаться.
fn tmp(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("yol_it_{}_{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text.as_bytes()).expect("не удалось записать временный файл");
}

fn read(path: &Path) -> String {
    String::from_utf8(std::fs::read(path).expect("нет выходного файла"))
        .expect("выходной файл не в UTF-8")
}

// ─────────────────────────────────────────────────────────────────────────────
//  Служебные ключи
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn version_prints_and_exits_zero() {
    let out = yol().arg("--version").output().expect("не запустился");
    assert!(
        out.status.success(),
        "код возврата: {:?}",
        out.status.code()
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("yol "), "вывод: {text:?}");
    assert!(
        text.trim().len() > "yol ".len(),
        "после «yol » должна быть версия: {text:?}"
    );
}

#[test]
fn help_prints_usage_and_exits_zero() {
    for flag in ["-h", "--help"] {
        let out = yol().arg(flag).output().expect("не запустился");
        assert!(out.status.success(), "{flag}: {:?}", out.status.code());
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("ИСПОЛЬЗОВАНИЕ"), "{flag}: {text:?}");
        assert!(text.contains("--ambiguous"), "{flag}: {text:?}");
    }
}

#[test]
fn unknown_flag_is_rejected() {
    let out = yol()
        .arg("--такого-ключа-нет")
        .output()
        .expect("не запустился");
    assert_eq!(out.status.code(), Some(2), "неизвестный ключ — это код 2");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("неизвестный ключ"), "stderr: {err:?}");
}

#[test]
fn unknown_encoding_is_rejected() {
    let out = yol()
        .args(["--in-enc", "крокозябра"])
        .output()
        .expect("не запустился");
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("кодировка"), "stderr: {err:?}");
}

// ─────────────────────────────────────────────────────────────────────────────
//  Файл → файл
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn file_to_file_is_yofied() {
    let src = tmp("in_utf8.txt");
    let dst = tmp("out_utf8.txt");
    write(&src, "Актер вышел на сцену, а береза стояла у елки.");

    let out = yol()
        .args(["-i"])
        .arg(&src)
        .args(["-o"])
        .arg(&dst)
        .arg("-q")
        .args(["--out-enc", "utf8"])
        .output()
        .expect("не запустился");

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(read(&dst), "Актёр вышел на сцену, а берёза стояла у ёлки.");

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
}

#[test]
fn out_encoding_cp1251() {
    let src = tmp("in_enc.txt");
    let dst = tmp("out_cp1251.txt");
    write(&src, "елка береза");

    let out = yol()
        .args(["-i"])
        .arg(&src)
        .args(["-o"])
        .arg(&dst)
        .arg("-q")
        .args(["--out-enc", "cp1251"])
        .output()
        .expect("не запустился");
    assert!(out.status.success());

    let bytes = std::fs::read(&dst).expect("нет выходного файла");
    assert_eq!(decode_cp1251(&bytes), "ёлка берёза");

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
}

#[test]
fn in_encoding_cp1251() {
    let src = tmp("in_cp1251.txt");
    let dst = tmp("out_from_cp1251.txt");
    std::fs::write(&src, encode_cp1251("елка береза")).expect("запись");

    let out = yol()
        .args(["-i"])
        .arg(&src)
        .args(["-o"])
        .arg(&dst)
        .arg("-q")
        .args(["--in-enc", "cp1251"])
        .args(["--out-enc", "utf8"])
        .output()
        .expect("не запустился");
    assert!(out.status.success());
    assert_eq!(read(&dst), "ёлка берёза");

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
}

#[test]
fn positional_arguments_work_as_input_and_output() {
    let src = tmp("pos_in.txt");
    let dst = tmp("pos_out.txt");
    write(&src, "елка");

    let out = yol()
        .arg(&src)
        .arg(&dst)
        .arg("-q")
        .args(["--out-enc", "utf8"])
        .output()
        .expect("не запустился");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(read(&dst), "ёлка");

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
}

// ─────────────────────────────────────────────────────────────────────────────
//  stdin → stdout
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn stdin_to_stdout_is_yofied() {
    let mut child = yol()
        .arg("-q")
        .args(["--out-enc", "utf8"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("не запустился");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all("Актер и елка".as_bytes())
        .expect("запись в stdin");

    let out = child.wait_with_output().expect("ожидание");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).expect("stdout не в UTF-8");
    assert_eq!(text, "Актёр и ёлка");
}

#[test]
fn dash_means_stdin_and_stdout() {
    let mut child = yol()
        .args(["-q", "--out-enc", "utf8", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("не запустился");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all("елка".as_bytes())
        .expect("запись в stdin");

    let out = child.wait_with_output().expect("ожидание");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8(out.stdout).expect("stdout не в UTF-8"), "ёлка");
}

// ─────────────────────────────────────────────────────────────────────────────
//  Настройки ёфикации из командной строки
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn ambiguous_ye_leaves_the_ambiguous_word_alone() {
    let src = tmp("amb_in.txt");
    let dst = tmp("amb_out.txt");
    write(&src, "все актер");

    let out = yol()
        .args(["-i"])
        .arg(&src)
        .args(["-o"])
        .arg(&dst)
        .arg("-q")
        .args(["--out-enc", "utf8", "--ambiguous", "ye", "--no-regexpr"])
        .output()
        .expect("не запустился");
    assert!(out.status.success());
    assert_eq!(read(&dst), "все актёр");

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
}

#[test]
fn ambiguous_yo_replaces_the_ambiguous_word() {
    let src = tmp("amb_in2.txt");
    let dst = tmp("amb_out2.txt");
    write(&src, "все актер");

    let out = yol()
        .args(["-i"])
        .arg(&src)
        .args(["-o"])
        .arg(&dst)
        .arg("-q")
        .args(["--out-enc", "utf8", "--ambiguous", "yo", "--no-regexpr"])
        .output()
        .expect("не запустился");
    assert!(out.status.success());
    assert_eq!(read(&dst), "всё актёр");

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
}

#[test]
fn missing_input_file_reports_an_error() {
    let out = yol()
        .args(["-i"])
        .arg(tmp("этого-файла-нет.txt"))
        .arg("-q")
        .output()
        .expect("не запустился");
    assert_eq!(out.status.code(), Some(1), "отсутствующий файл — это код 1");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("не удалось прочитать"), "stderr: {err:?}");
}
