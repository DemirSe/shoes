# Operasyon Runbook'u

Bu belge canlı sunucu ve yönetim prosedürlerini tanımlar.
**Kural:** Dağıtım, canlı bakım, yeniden başlatma ve ayar değişikliği için önce
sahip onayı alınır. APT işlemleri elle yapılır; otomatik yeniden başlatma kapalıdır.

## Sunucu düzeni (VPS)

| Öğe | Değer |
|---|---|
| Servis | `shoes.service` (kullanıcı: `xray`) |
| Çalıştırma | `/usr/local/bin/shoes --no-reload --production-profile /usr/local/etc/shoes/config.yaml` |
| Yapılandırma | `root:xray`, mod **640** — okunmaz, kopyalanmaz, raporlanmaz; yalnız SHA-256 tutulur |
| Bakım dizini | `/var/lib/shoes-maintenance/<id>/` (rollback + receipt) |
| Birim yedeği | `unit-profile-*` dizinindeki `shoes.service.before` |
| Dinleyici | Yalnız açık genel IPv4 TCP 443 |
| Kaynaklar | ~1 GB RAM; `LimitNOFILE=65536` |

`<VPS_IP>` yer tutucudur; gerçek adres ve SSH ayrıntıları yalnız yerel kayıtta
(`~/.pi/agent/server-ssh.json`).

## Deploy prosedürü

1. **Yerel release derlemesi** (izole toolchain, çevrimdışı):
   `cargo build -p shoes --bin shoes --release --locked --offline`
2. **SHA-256 hesapla** ve `scp` ile VPS'te `/tmp`'ye kopyala; hedefte hash'i doğrula.
3. **Temel durumu kaydet:** canlı binary hash'i, unit hash'i, config hash'i,
   `systemctl is-active`, `NRestarts`.
4. **Sahnele:** `/var/lib/shoes-maintenance/release-<short>-<ts>/`
   - `shoes.new` (yeni binary)
   - `rollback.bin` (mevcut canlı binary'nin kopyası)
5. **Atomik kur:** `install -m 755 shoes.new /usr/local/bin/shoes.new` +
   `mv /usr/local/bin/shoes.new /usr/local/bin/shoes` (aynı dosya sistemi).
6. **Yeniden başlat:** `systemctl restart shoes`.
7. **Sağlık kontrolü:** servis `active` **ve** `ss -ltn` ile 443 dinliyor **ve**
   `NRestarts=0`. Başarısızsa otomatik rollback (aşağıdaki adım).
8. **Değişmezlik:** unit ve config hash'leri öncekiyle **aynı** olmalı.
9. **Receipt:** `receipt.txt` içine tarih, binary/unit/config hash'leri, rollback
   komutu yazılır.
10. **Eski kayıtları temizle:** yalnız en güncel release dizini + unit yedeği kalır.

## Rollback

```bash
# VPS'te, <DIR> = ilgili bakım dizini
sudo install -m 755 <DIR>/rollback.bin /usr/local/bin/shoes.rollback
sudo mv /usr/local/bin/shoes.rollback /usr/local/bin/shoes
sudo systemctl restart shoes
# Sağlık kontrolünü tekrarla
```

Deploy script'i sağlık kontrolü başarısız olursa rollback'i **otomatik** uygular.

## Yönetim SSH kuralları

- TUN, VPS adresini kendi üzerinden yönlendirebilir; bu yüzden SSH **WiFi
  arayüzüne bağlanır** (`BindInterface`). Böylece servis restart'ı veya TUN
  değişimi oturumu kesmez.
- Uzun oturumlarda `ServerAliveInterval`/`ServerAliveCountMax` kullanılır
  (hotspot kopmalarında asılı kalmayı önler).
- Kimlik bilgileri komut satırına gömülmez; yerel anahtar dosyası kullanılır.

## İzleme

Yerel script'ler (`~/.local/state/shoes-live/`):

| Script | Nerede çalışır | Ölçtüğü |
|---|---|---|
| `server-monitor.sh` | VPS (salt okunur) | CPU %, RSS, thread, eşzamanlı 443 bağlantısı, backlog, retrans paket/bayt, ERROR sayısı |
| `client-probe.sh` | Laptop | Tünel RTT (ping) + gstatic TTFB (tünel üzerinden) |

Tipik sağlıklı aralıklar (ölçülen): CPU %0,1–2,8; backlog sürekli 0; TTFB
~0,24–0,32 sn; ping ~20–50 ms (mobil yola göre). Bu değerler garanti değil,
gözlem kaydıdır.

## Bellek operasyonu

- Bağlantı patlaması sonrası glibc serbest sayfaları tutabilir; RSS yüksek kalır
  (sızıntı değildir — aynı stres tekrarlanınca RSS artmaz).
- Otomatik çözüm: `src/memory.rs` heap trim (5 dk, RSS > 64 MB, bağlantı < 128).
- Acil durumda RSS sıfırlamak için onaylı yeniden başlatma yapılabilir
  (servis restart'ı bağlantıları keser; kısa kesinti).

## Acil durum kontrol listesi

1. Servis durumu: `systemctl is-active shoes`, `NRestarts`.
2. Dinleyici: `ss -ltn | grep 443`.
3. Son hatalar: `journalctl -u shoes --since "-5 min" -o cat`.
4. Kaynak: CPU/RSS/thread, eşzamanlı bağlantı, backlog (server-monitor.sh).
5. Gerekirse rollback (yukarıdaki adım) — sonra sağlık kontrolü.
6. Yaşanan olayı yerel kayda işle (kimlik bilgisi ve ham config olmadan).

## CI ve ağ politikası

- CI **çevrimdışıdır**; tüm işler elle (`workflow_dispatch`), önceden hazırlanmış
  self-hosted runner'larda çalışır; kod indirmez, ağ erişimi yapmaz.
- **Hiçbir workflow VPS'e bağlanmaz.**
- Ayrıntı: [.github/OFFLINE-CI.md](../.github/OFFLINE-CI.md).

## İlgili belgeler

- Deploy sonrası ölçüm: [performans-bellek.md](performans-bellek.md)
- Sorun giderme: [sorun-giderme.md](sorun-giderme.md)
