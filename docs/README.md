# shoes (fork) — Uçtan Uca Dokümantasyon

Bu klasör, bu depodaki `shoes` fork'unun uçtan uca belgelerini içerir.
Kapsam: istemci cihazlar (telefon + laptop), ağ yolu, sunucu (VPS), mimari,
protokoller, operasyon, test, kararlar, performans geçmişi, güvenlik sınırları
ve sorun giderme.

## İçindekiler

| Belge | İçerik |
|---|---|
| [genel-bakis.md](genel-bakis.md) | Proje nedir, kısıtlar, uçtan uca topoloji (telefon → laptop → sunucu) |
| [mimari.md](mimari.md) | Kod mimarisi, modül haritası, başlangıç/reload/kapanış akışı |
| [protokoller.md](protokoller.md) | VLESS, REALITY, Vision, XUDP, UoT, H2MUX akışları |
| [konfigurasyon.md](konfigurasyon.md) | Yapılandırma özeti ve üretim profili |
| [operasyon.md](operasyon.md) | Deploy, rollback, sağlık kontrolü, izleme, acil durum |
| [test-ve-ci.md](test-ve-ci.md) | Yerel test komutları, çevrimdışı CI kuralları |
| [kararlar.md](kararlar.md) | Tutulan/çıkarılan özellikler ve sahip kısıtları |
| [performans-bellek.md](performans-bellek.md) | Perf turları, sertleştirmeler, heap trim |
| [guvenlik-ve-limitler.md](guvenlik-ve-limitler.md) | Doğrulanan/doğrulanmayan iddialar, bilinen sınırlar |
| [sorun-giderme.md](sorun-giderme.md) | Sık hatalar, teşhis komutları, ölçüm tuzakları |

## Hangi işlem için nereden başlanır?

| İşlem | Esas kaynak |
|---|---|
| Geliştirme ortamı, test ve release hazırlığı | [CONTRIBUTING.md](../CONTRIBUTING.md) |
| CLI kullanımı | [README.md](../README.md#usage) |
| YAML alanı / varsayılanı | [CONFIG.md](../CONFIG.md) |
| Üretim profili şartları / iskeleti | [konfigurasyon.md](konfigurasyon.md) |
| Dağıtım / sağlık / rollback / yedek saklama | [operasyon.md](operasyon.md) |
| Güncel kapsam ve ertelenen işler | [kararlar.md](kararlar.md) |

Alan tanımları, test komutları ve deploy adımları kendi esas belgelerinde güncellenir.
Özet belgeler bunları yeniden çoğaltmaz; ilgili kaynağa bağlanır.
Eski changelog girdileri ve tarihli ölçümler geçmiş kayıttır.
Tarihsel öneriler güncel davranış veya canlı durum kanıtı sayılmaz.
Kaldırılan inceleme kaydı Git geçmişinden `git log --all -- report.md` ile bulunabilir.

Depodaki diğer belgeler: [CONFIG.md](../CONFIG.md) (tam YAML referansı),
[SERVER-FEATURE-DECISIONS.md](../SERVER-FEATURE-DECISIONS.md) (özellik kararları),
[.github/OFFLINE-CI.md](../.github/OFFLINE-CI.md) (CI ön koşulları).

## Hızlı gerçekler

- Fork tabanı: `shoes` 0.3.2 (Rust). Sunucu tarafı tek profilde çalışır.
- Protokol: VLESS + REALITY + Vision, TCP 443; UDP/XUDP aynı TCP tüneli içinde.
- REALITY hedefi ve SNI: yalnız `whatsapp.net`.
- Dinleme: yalnız açık genel IPv4 TCP 443; UDP 443 dinleyicisi yoktur.
- Kaldırılanlar geri eklenmez: VMess, Trojan, Shadowsocks/ShadowTLS, HTTP/SOCKS.
- TUN açık ve bypass olmaması sahip gereksinimidir; mevcut istisnalar
  [doğrulama sınırlarında](guvenlik-ve-limitler.md) ayrıca belirtilir.
- CI çevrimdışıdır, elle çalıştırılır (`workflow_dispatch`) ve VPS'e bağlanmaz.
- Dağıtım elle ve atomiktir: hazırla → `install` + `mv` → `systemctl restart` → sınırlı sağlık gözlemi.
  Her dağıtım `rollback.bin` bırakır; doğrulama başarısızsa operatör elle geri döner.
- Otomatik dağıtım aracı yoktur (2026-10-16 sahip kararı); adımlar [operasyon.md](operasyon.md) içindedir.
  Otomatik rollback iddiası yoktur; dağıtım yedekleri otomatik silinmez,
  temizlik ayrıca onay gerektirir.

## Durum etiketleri

- **Yerelde uygulandı:** kodda bulunur; canlıya dağıtıldığı anlamına gelmez.
- **Yerelde doğrulandı:** kapsamı ve tarihi belirtilen kontrollerden geçti.
- **Canlıya dağıtıldı:** onaylı dağıtımın tarihli receipt'i gerekir.

Bu düzenleme ve yerel doğrulamalar canlı doğrulama değildir.

## Hassas ayrıntılar

Bu belgeler herkese açık depoda tutulabilir; **sunucu adresi, kimlik bilgileri
(UUID/anahtar/şifre) ve ham yapılandırma içermez**. Gerçek adres ve bağlantı
ayrıntıları yalnız yerel kayıtlarda durur:

- `~/.pi/agent/server-ssh.json` — SSH bağlantı ayrıntıları
- `~/.local/state/shoes-live/` — canlı ölçüm ve karar kayıtları

Bu belgelerdeki `<VPS_IP>` yer tutucusu gerçek sunucu adresinin yerinedir.
