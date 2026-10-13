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

Depodaki diğer belgeler: [CONFIG.md](../CONFIG.md) (tam YAML referansı),
[report.md](../report.md) (inceleme ve karar kayıtları),
[SERVER-FEATURE-DECISIONS.md](../SERVER-FEATURE-DECISIONS.md) (özellik kararları),
[.github/OFFLINE-CI.md](../.github/OFFLINE-CI.md) (CI ön koşulları).

## Hızlı gerçekler

- Fork tabanı: `shoes` 0.3.2 (Rust). Sunucu tarafı tek profilde çalışır.
- Protokol: VLESS + REALITY + Vision, TCP 443; UDP/XUDP aynı TCP tüneli içinde.
- REALITY hedefi ve SNI: yalnız `whatsapp.net`.
- Dinleme: yalnız açık genel IPv4 TCP 443; UDP 443 dinleyicisi yoktur.
- Kaldırılanlar geri eklenmez: VMess, Trojan, Shadowsocks/ShadowTLS, HTTP/SOCKS.
- TUN istemcide açık kalır; sahip kuralı olarak bypass kuralı yoktur.
- CI çevrimdışıdır, elle çalıştırılır (`workflow_dispatch`) ve VPS'e bağlanmaz.
- Deploy atomiktir: hazırla → `install` + `mv` → `systemctl restart` → sağlık kontrolü.
  Her deploy `rollback.bin` bırakır.

## Hassas ayrıntılar

Bu belgeler herkese açık depoda tutulabilir; **sunucu adresi, kimlik bilgileri
(UUID/anahtar/şifre) ve ham yapılandırma içermez**. Gerçek adres ve bağlantı
ayrıntıları yalnız yerel kayıtlarda durur:

- `~/.pi/agent/server-ssh.json` — SSH bağlantı ayrıntıları
- `~/.local/state/shoes-live/` — canlı ölçüm ve karar kayıtları

Bu belgelerdeki `<VPS_IP>` yer tutucusu gerçek sunucu adresinin yerinedir.
