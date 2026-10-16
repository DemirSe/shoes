# Test ve CI

## Yerel doğrulama

Toolchain, ortam değişkenleri, önceden sağlanmış sing-box eşi ve esas komut listesi
[CONTRIBUTING.md](../CONTRIBUTING.md) içindedir. Komutları oradan kullanın;
bu belge test kapsamı ve CI sınırlarını açıklar.

| Değişiklik | Kabul kapsamı |
|---|---|
| Rust kodu / protokol davranışı | Birim, FFI, entegrasyon, Clippy, biçim ve gerekiyorsa release |
| Yalnız dokümantasyon | Bağlantı, örnek/kod tutarlılığı ve whitespace kontrolü |
| Allocator / sıcak yol | İlgili regresyonlar; performans iddiası varsa ayrıca eşleşen release ölçümü |

Her Cargo build/test/lint komutu `--locked --offline` kullanır. Hedef dizini repo dışında tutulur.
`cargo test --all-features` kullanılmaz; dış ağ, root/TUN ve stres testlerini de açar.
Entegrasyon eşi eksikse ilgili testler başarısız olur; atlanmış sayılmaz.
Fixture çıktıları sentetik yapılandırmalar içerebilir; raporlara ham çıktıyı değil sonuç özetini ekleyin.
Ortak kod ayrı hedeflerde tekrar sınanır; sayılar benzersiz test toplamı değildir.

## İsteğe bağlı allocator bellek regresyonu

Bu test varsayılan pakette `ignored` kalır. GNU/Linux CLI'da yaklaşık 256 MiB geçici bellek kullanır;
derleme dışında 180 saniyeye kadar bekleyebilir. Normal CI veya üretim sağlık kontrolü değildir.
Geliştirme rehberindeki komut-yerel ortamla yalnız bu testi seçin:

```bash
cargo test -p shoes --release --locked --offline --bin shoes \
  allocator::tests::background_reclaims_unused_buffers_with_workers_alive \
  -- --ignored --exact --nocapture --test-threads=1
```

Tüm ignored/stres testlerini topluca açmayın. Ölçümün sınırları:
[performans-bellek.md](performans-bellek.md).

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
