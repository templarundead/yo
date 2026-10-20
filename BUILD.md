# Сборка YOL

Итоговая цель — **один исполняемый файл** `yol` (или `yol.exe`), в который
встроены оба словаря (`assets/yox.dat`, `assets/yo_regexpr.dat`). Никаких
внешних файлов, никакой установки: бинарник можно просто скопировать на другую
машину.

---

## 0. Что понадобится везде

### Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup update stable
```

Необходимая версия — **1.76+** (используются `let-else` и раздельные захваты
замыканий edition 2021). Проверить:

```bash
rustc --version
```

### Рендереры (две фичи eframe)

В `Cargo.toml` включены **обе** фичи: `glow` (OpenGL, идёт по умолчанию у
eframe) и `wgpu` (WebGPU/Vulkan/Metal/DX12). С обеими фичами eframe по
умолчанию выбирает **WGPU**; переключить на OpenGL можно переменной окружения:

```bash
YOL_RENDERER=glow ./yol    # OpenGL / OpenGL ES — самый совместимый вариант
./yol                      # WGPU (Vulkan / Metal / DX12)
```

Если нужен только один бэкенд (меньше размер бинарника), поправьте
`Cargo.toml`:

```toml
eframe = { version = "0.31", default-features = false, features = [
    "accesskit", "default_fonts", "wayland", "x11", "glow",   # только OpenGL
] }
```

### Системные зависимости (только для сборки)

`eframe` тянет за собой `winit`, а тот через `pkg-config` ищет X11 / Wayland.
На Linux сборка потребует dev-пакетов.

**Debian / Ubuntu:**

```bash
sudo apt update
sudo apt install -y build-essential pkg-config \
    libx11-dev libxcursor-dev libxrandr-dev libxi-dev \
    libxkbcommon-dev libwayland-dev \
    libgl1-mesa-dev libglu1-mesa-dev \
    libssl-dev
```

**Alpine:**

```bash
apk add --no-cache build-base pkgconfig \
    xorgproto libx11-dev libxcursor-dev libxrandr-dev libxi-dev \
    libxkbcommon-dev wayland-dev wayland-protocols \
    mesa-dev glu-dev openssl-dev
```

> Если нужна «безголовая» сборка (только консольный режим без GUI), можно
> отключить X11/Wayland в `Cargo.toml`, но это уже не штатный путь.

---

## 1. Linux x86_64 (Debian / Ubuntu) — нативная сборка

```bash
cd yo
cargo build --release
ls -lh target/release/yol
./target/release/yol
```

### Полностью статический бинарник (musl)

glibc-статику получать не рекомендуется (ломается NSS, DNS, dlopen), поэтому
для статики используем musl:

```bash
rustup target add x86_64-unknown-linux-musl
sudo apt install -y musl-tools          # даёт musl-gcc

cargo build --release --target x86_64-unknown-linux-musl
file target/x86_64-unknown-linux-musl/release/yol
#   ELF 64-bit LSB executable, statically linked …
```

Запустить на любой машине x86_64:

```bash
scp target/x86_64-unknown-linux-musl/release/yol user@host:~/
```

> `.cargo/config.toml` уже содержит `-C target-feature=+crt-static` и
> `linker = "rust-lld"` для musl-целей, поэтому дополнительных ключей
> указывать не нужно. `rust-lld` ставится вместе с цель-тулчейном
> (`rustup component add rust-lld`), либо удалите строку `linker` — тогда
> будет использован системный `musl-gcc`.

---

## 2. Linux ARM64 — Orange Pi 5+ / Rockchip RK3588

### Вариант А: собираем прямо на плате (проще всего)

```bash
# Debian 12 (bookworm) / Armbian / Ubuntu 22.04+ на самой плате
sudo apt update
sudo apt install -y build-essential pkg-config \
    libx11-dev libxcursor-dev libxrandr-dev libxi-dev \
    libxkbcommon-dev libwayland-dev libgl1-mesa-dev libssl-dev

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
cd yo
cargo build --release
./target/release/yol
```

Сборка на RK3588 (8 ядер) занимает примерно 4–8 минут.

**Статика на плате (musl):**

```bash
rustup target add aarch64-unknown-linux-musl
sudo apt install -y musl-tools
cargo build --release --target aarch64-unknown-linux-musl
```

### Вариант Б: кросс-компиляция с x86_64-машины (быстрее)

**Через `cross` (рекомендуется — всё внутри контейнера):**

```bash
cargo install cross --locked
# нужен Docker или Podman

# glibc (динамический бинарник для Debian/Ubuntu ARM64)
cross build --release --target aarch64-unknown-linux-gnu

# musl (статический — работает на любой ARM64-системе, включая Alpine)
cross build --release --target aarch64-unknown-linux-musl
```

**Через системный кросс-линкер:**

```bash
# Debian/Ubuntu
sudo apt install -y gcc-aarch64-linux-gnu g++-aarch64-linux-gnu
rustup target add aarch64-unknown-linux-gnu

mkdir -p .cargo/aarch64-sysroot   # sysroot с libx11 и т. д. для ARM64
# самый удобный путь — взять готовый sysroot из docker-образа ARM64,
# либо использовать cross (см. выше).

cargo build --release --target aarch64-unknown-linux-gnu \
    --config 'target.aarch64-unknown-linux-gnu.linker="aarch64-linux-gnu-gcc"'
```

**Через `cargo-zigbuild`** (кросс-линкер без возни с sysroot, хорошо дружит с musl):

```bash
pip install ziglang          # или: snap install zig
cargo install cargo-zigbuild
rustup target add aarch64-unknown-linux-gnu

cargo zigbuild --release --target aarch64-unknown-linux-gnu
cargo zigbuild --release --target aarch64-unknown-linux-musl.2.17
```

### Запуск на Orange Pi 5+

```bash
scp target/aarch64-unknown-linux-*/release/yol orangepi@opi5:~/
ssh orangepi@opi5 ./yol
```

Если графическая подсистема не поднимается (нет Vulkan на RK3588 в вашей
прошивке), переключите рендерер на OpenGL:

```bash
YOL_RENDERER=glow ./yol
# и/или принудительно выбрать GL-бэкенд wgpu:
WGPU_BACKEND=gl ./yol
# и/или X11 вместо Wayland:
WINIT_UNIX_BACKEND=x11 ./yol
```

`YOL_RENDERER=glow` заставляет `eframe` использовать Glow (OpenGL ES) вместо
WGPU — это самый совместимый вариант для ARM-плат.

---

## 3. Alpine Linux ARM64 (и x86_64) — статический бинарник

```bash
apk add --no-cache rust cargo build-base pkgconfig \
    xorgproto libx11-dev libxcursor-dev libxrandr-dev libxi-dev \
    libxkbcommon-dev wayland-dev wayland-protocols mesa-dev

cd yo
cargo build --release
ldd target/release/yol         # → statically linked (musl по умолчанию на Alpine)
```

Результат — полностью статический ELF без внешних `.so`, который одинаково
работает на Alpine, Debian, Ubuntu и любой другой ARM64-системе.

---

## 4. Windows x86_64

### Вариант А: MSVC (Visual Studio)

1. Установите [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/)
   с рабочей нагрузкой «Разработка классических приложений на C++».
2. Установите Rust с `x86_64-pc-windows-msvc` (по умолчанию):

   ```powershell
   winget install Rustlang.Rustup
   rustup default stable-msvc
   ```

3. Соберите:

   ```powershell
   cargo build --release
   .\target\release\yol.exe
   ```

`yol.exe` зависит только от системных DLL Windows (user32, gdi32, opengl32 /
dxgi) — всё остальное, включая оба словаря, скомпоновано внутрь.
Дополнительно распространять ничего не нужно.

### Вариант Б: GNU (MinGW) — кросс-сборка с Linux

```bash
# Debian/Ubuntu
sudo apt install -y mingw-w64
rustup target add x86_64-pc-windows-gnu

cd yo
cargo build --release --target x86_64-pc-windows-gnu \
    --config 'target.x86_64-pc-windows-gnu.linker="x86_64-w64-mingw32-gcc"'
# → target/x86_64-pc-windows-gnu/release/yol.exe
```

### Вариант В: через `cross` / `cargo-zigbuild`

```bash
cross build --release --target x86_64-pc-windows-gnu
cargo zigbuild --release --target x86_64-pc-windows-gnu
```

### Windows ARM64

```powershell
rustup target add aarch64-pc-windows-msvc
cargo build --release --target aarch64-pc-windows-msvc
```

---

## 5. macOS (для полноты)

```bash
rustup target add x86_64-apple-darwin aarch64-apple-darwin
cargo build --release --target $(uname -m)-apple-darwin
```

---

## 6. Профили сборки

| Команда | Профиль | Когда нужен |
|---|---|---|
| `cargo build --release` | `release` | **основной**: `opt-level=3`, `lto=true`, `codegen-units=1`, `panic=abort`, `strip=true` |
| `cargo build --release-lite` | `release-lite` | отладка оптимизированной сборки: thin LTO, символы, unwind |
| `cargo build --profile minimum` | `minimum` | минимальный размер: `opt-level="z"` |

Все профили наследуют `strip`/`debug` из `release` (кроме `release-lite`).

---

## 7. Тесты и бенчмарк

```bash
cargo test --all              # юнит-тесты + интеграционные (tests/) + док-тесты
cargo test --release --all    # то же в release — цифры ближе к боевым

# только интеграционные тесты
cargo test --test yofication
cargo test --test dictionary
cargo test --test encoding
cargo test --test cli

# прикидка скорости на большом файле
head -c 100000000 /dev/urandom | base64 > /tmp/big.txt   # 100 МБ
./target/release/yol -i /tmp/big.txt -o /tmp/big.out.txt
```

Замер времени «холодного старта» до появления окна:

```bash
./target/release/yol --version      # консольный путь, без GUI
cargo test --release -- --nocapture dictionary_is_loaded_and_sorted
```

> Интеграционные тесты из `tests/` работают с **библиотекой** `yol`
> (`src/lib.rs`), поэтому ядро обязано оставаться библиотечной целью:
> если вынести `mod yo_core` обратно в `main.rs`, `use yol::yo_core::…`
> в `tests/` перестанет собираться.

### Линт и форматирование перед пушем

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

Ровно это делает Stage 1 пайплайна, поэтому локальный прогон экономит
круг CI. См. `.github/workflows/ci.yml`.

---

## 8. Устранение неполадок

| Симптом | Лечение |
|---|---|
| `pkg-config exited with status code 1` / `cannot find -lX11` | не стоят dev-пакеты X11/Wayland (см. §0) |
| Окно чёрное / не рисуется на ARM-плате | `YOL_RENDERER=glow ./yol` или `WGPU_BACKEND=gl ./yol` |
| Приложение не стартует под Wayland | `WINIT_UNIX_BACKEND=x11 ./yol` |
| `linker 'rust-lld' not found` | `rustup component add rust-lld` или уберите `linker` из `.cargo/config.toml` |
| Бинарник больше ожидаемого | используйте `--profile minimum`; проверьте, что не собираете с `debug = true` |
| Кракозябры вместо русского текста | откройте файл с правильной кодировкой: `--in-enc cp1251` |
| `yol.exe` не запускается на старой Windows | соберите под `x86_64-pc-windows-gnu` — он не требует VC++ Redist |

---

## 9. Что именно попадает в бинарник

```
target/release/yol
├── код egui/eframe/winit/wgpu/glow   ← GUI
├── код regex                          ← движок регулярных выражений
├── assets/yox.dat          (1.8 МБ)   ┐ include_bytes!
└── assets/yo_regexpr.dat   (1.2 КБ)   ┘
```

`build.rs` проверяет наличие обоих файлов на этапе компиляции и уронит сборку,
если какой-то из них пропал — лучше упасть при сборке, чем получить бинарник
с пустым словарём.
