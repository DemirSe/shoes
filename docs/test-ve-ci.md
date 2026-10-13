# Test ve CI

## Yerel toolchain

İzole Rust toolchain: `/home/demir/.local/share/shoes-toolchain`.
Ortam değişkenleri **komut bazında** verilir; shell profilleri değiştirilmez:

```bash
CARGO_HOME=/home/demir/.local/share/shoes-toolchain/cargo \
RUSTUP_HOME=/home/demir/.local/share/shoes-toolchain/rustup \
PATH=/home/demir/.local/share/shoes-toolchain/cargo/bin:$PATH \
cargo test --locked --offline
```

- Her komuta `--locked --offline`: asla crate veya test eşi indirilmez.
- Derleme hedefi (`CARGO_TARGET_DIR`) repo dışında tutulur. `/tmp` küçük bir
  tmpfs'tir; büyük derlemeler için `/home` veya `~/.cache` kullanılır.

## Komutlar

| Amaç | Komut |
|---|---|
| Birim testleri | `cargo test --workspace --locked --offline --lib --bins` |
| Entegrasyon | `cargo test -p shoes --locked --offline --features integration-tests --test integration -- --test-threads=1` |
| FFI | `cargo test -p shoes --locked --offline --lib --features ffi -- --test-threads=1` |
| Lint | `cargo clippy --workspace --locked --offline --all-targets --all-features -- -D warnings` |
| Biçim | `cargo fmt --all -- --check` |
| Dağıtım derlemesi | `cargo build -p shoes --bin shoes --release --locked --offline` |

### Önemli notlar

- Bazı entegrasyon testleri gerçek bir sing-box eşi ister
  (`SHOES_TEST_SING_BOX_BIN`). Eş yoksa **atlanmaz, başarısız olur**.
- `cargo test`'e `--all-features` **verilmez**: `external-network-tests`,
  `privileged-tests` ve `stress-tests` özelliklerini de açar (dış servisler,
  root/TUN, benchmark'lar) — bunlar çevrimdışı CI setinin dışındadır.
- Test hedefleri ortak kod sınayan paketler içerir; toplamları benzersiz test
  sayısı olarak toplanmamalıdır.

## Çevrimdışı CI

Kurallar [.github/OFFLINE-CI.md](../.github/OFFLINE-CI.md) içinde:

- Tüm işler **elle** (`workflow_dispatch`), önceden hazırlanmış self-hosted
  runner'larda (`shoes-offline-linux` / `shoes-offline-macos`).
- İşler kod **checkout etmez ve indirmez**; önceden sağlanan commit'i doğrular
  (`HEAD == GITHUB_SHA`) ve takip edilen dosyalarda kirli durumu reddeder.
- Runner ağ çıkışı engellenir; loopback testleri açık kalır.
- **VPS erişimi hiçbir işte yoktur**; izolasyon denemeleri VPS üzerinde yapılmaz.
- Eksik araç/önbellek işi başarısız kılar; indirme tetiklemez.
- Eserler runner'da kalır; otomatik yükleme/yayın yoktur.

Workflow'lar: `test.yml`, `lint.yml`, `build.yml`, `mobile.yml`.
Hiçbiri dağıtım yapmaz, sunucuyu ayarlamaz, güncellemez veya yeniden başlatmaz.

## İlgili belgeler

- Karar kayıtları: [kararlar.md](kararlar.md)
- Operasyon: [operasyon.md](operasyon.md)
