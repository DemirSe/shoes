# Sorun Giderme

## Günlükte sık görülen (ve zararsız) hatalar

Aşağıdakiler internet tarayıcılarından gelen rastgele bağlantı denemeleridir;
servis hatası değildir:

```
ERROR shoes::tcp::tcp_server] <ip>:<port> finished with error:
  failed to setup server stream: expected client handshake
ERROR shoes::tcp::tcp_server] <ip>:<port> finished with error:
  failed to setup server stream: EOF while reading
```

- `expected client handshake`: TLS/REALITY el sıkışması olmayan bağlantı.
- `EOF while reading`: yarıda kesilen bağlantı (tarayıcı/port taraması).
- Bunlar fail-closed davranışın normal parçasıdır; sürerse yalnız gürültüdür.

## Temel teşhis komutları

```bash
# Servis ve yeniden başlatma sayacı
systemctl is-active shoes && systemctl show shoes -p NRestarts

# Dinleyici
ss -ltn | grep 443

# Son hatalar (salt okunur)
journalctl -u shoes --since "-5 min" -o cat | tail -n 20

# Kaynak özeti (60 sn'de bir loglanır): connections/streams/quic/per_ip
journalctl -u shoes --since "-2 min" -o cat | grep -i resource | tail -n 3

# İzleme script'i (VPS'te, salt okunur)
bash server-monitor.sh 5 12
```

## Ölçüm tuzakları

| Tuzak | Doğrusu |
|---|---|
| `ss -un` bağlanmamış UDP soketlerini gizler | `ss -tulpn` kullan |
| `rchar` soket okumalarını saymaz | İndirmede `bytes_sent` / eth0 say |
| Yeniden başlatma sonrası ilk TTFB yüksek (~2 sn) | Soğuk başlangıç; 2-3 ölçüm al |
| Hotspot istemcisi eşzamanlılığı sınırlar (tepe ~24 gözlenebilir) | Tepe eşzamanlılığı ayrıca ölçmeden yorum yapma |
| Uzun SSH oturumu hotspot kopmasında asılı kalır | `ServerAliveInterval=15`, `ServerAliveCountMax=4` |
| TUN, VPS adresini kendi üzerinden yönlendirebilir | SSH'i WiFi arayüzüne bağla (`BindInterface`) |

## Bellek

- **RSS yüksek kaldı:** glibc arena rezervi olabilir; sızıntı değildir.
  Doğrulama: aynı stresi tekrarla; RSS artmıyorsa rezervdir.
- **Trim çalışmıyor gibi görünüyor:** koşullar — 5 dk aralık, RSS > 64 MB,
  aktif bağlantı < 128. Normal kullanım 20-40 bağlantı açar; eşik altında trim
  yapılmaması **doğru davranıştır**.
- **Trim'i kanıtlamak:** `perf trace -e madvise` ile tek patlamada
  `MADV_DONTNEED` çağrıları görülür.
- **Acil sıfırlama:** onaylı `systemctl restart shoes` (kısa kesinti).

## Reload ve yapılandırma

- Geçersiz yapılandırma reload'ı mevcut nesli **korur**; hata loglanır.
- Adres ailesi/wildcard kapsamı değişen geçişler (ör. `[::]:443` ↔ `0.0.0.0:443`)
  reddedilir; yeniden başlatma gerekir.
- Canlı birim `--no-reload` çalışır; bu yüzden dosya değişikliği kendiliğinden
  uygulanmaz — bilinçli tercih.
- `--dry-run` geçersiz yapılandırmada sıfır dışı kod döner; deploy öncesi doğrulama
  için kullanılabilir.

## Üretim profili redleri

`--production-profile` şu durumlarda yapılandırmayı reddeder:

- Dinleme adresi açık genel IPv4 değil veya port 443 değil.
- `transport: udp` kullanılmış.
- REALITY hedefi/SNI `whatsapp.net` değil (alt alan/başka kapak dahil).
- Opsiyonel VLESS fallback yapılandırılmış.

## TUN

- TUN değişiklikleri canlıda uygulanamaz; `server_runtime` TUN canlı değişimini
  reddeder. Onaylı yeniden başlatma gerekir.

## Deploy sorunları

- **Hash uyuşmazlığı:** dosya aktarımı bozulmuş; yeniden kopyala, hash doğrula.
- **Sağlık kontrolü başarısız:** script otomatik rollback uygular; `receipt.txt`
  ve `rollback.bin` bakım dizininde kalır.
- **Unit/config hash'i değişti:** beklenmedik; deploy durdurulmalı ve incelenmeli.
- **NRestarts > 0:** beklenmedik yeniden başlatma; journal ve kaynak durumuna bak.

## CPU yükselmesi

- Tarayıcı/port taraması dalgası: `expected client handshake` hatası sayısı artar;
  kabul reddi freni (100 ms) bütçe doluyken CPU yakmayı önler.
- Gerçek yük: eşzamanlı bağlantı ve aktarım hacmine bak (server-monitor.sh).
- Sıcak sembol analizi için `perf` gerekir; ölçüm dışı dönemlerde kurulu tutulmaz.

## Kayıtlar nerede?

- `report.md` — inceleme ve karar kayıtları (depoda).
- `SERVER-FEATURE-DECISIONS.md` — özellik kararları (depoda).
- `~/.local/state/shoes-live/` — canlı ölçüm, izleme script'leri, karar kayıtları.
- `~/.pi/agent/server-ssh.json` — bağlantı ayrıntıları (yerel).

## İlgili belgeler

- Operasyon: [operasyon.md](operasyon.md)
- Güvenlik sınırları: [guvenlik-ve-limitler.md](guvenlik-ve-limitler.md)
