# Sorun Giderme

## Günlükte sık görülen bağlantı hataları

Bağlantı kapanış hataları tarama veya yarıda kesilen gerçek istemci bağlantılarından gelebilir.
Tek başına servis arızasını veya tarayıcı kaynağını kanıtlamaz.
Yeni kaynak sürümü, her bağlantı için adres ve ham hata yazmak yerine sabit kategorili özet üretir:

```
ERROR shoes::connection_errors] Connection close error counts (not service failures): InvalidData=1 UnexpectedEof=0 ConnectionReset=0 ConnectionAborted=0 TimedOut=0 ConnectionRefused=0 BrokenPipe=0 Other=0
```

- İlk hata hemen raporlanır; sonraki özetler arasında en az 30 saniye vardır.
- Sayaçlar süreç genelinde TCP/Unix dinleyicileri ve reload nesilleri arasında paylaşılır.
- Özet, önceki özette sıfırlanan sayaçlardan sonra biriken kapanış hatalarını içerir; boş özet üretilmez.
- Zamanlayıcı yoktur: bekleyen sayılar sonraki hatada, süre dolmuşsa raporlanır. Sessizlikte yazılmaz.
- Reload kapıyı veya bekleyen sayıları sıfırlamaz. Süreç çıkışında bekleyen sayılar kaybolabilir.
- ERROR düzeyi, mevcut varsayılan filtrede özeti görünür tutar; her sayı servis arızası değildir.
- DEBUG ayrıntısı yalnız hata kategorisidir; adres, ham hata metni veya paket verisi içermez.
- Bu sınırlama yalnız dış TCP/Unix kapanış kayıtlarını kapsar; bütün loglar için küresel kota değildir.
- Gerçek istemci başarısıyla karşılaştırın; sürekli hatayı otomatik olarak zararsız saymayın.

Linux stderr tanımlayıcısı `JOURNAL_STREAM` aygıt/inode kimliğiyle eşleşirse her kayda
systemd seviye öneki eklenir: ERROR `<3>`, WARN `<4>`, INFO `<6>`, DEBUG/TRACE `<7>`.
`SyslogLevelPrefix=yes` gereklidir (systemd varsayılanı); canlı ayar bu değişiklikte doğrulanmadı.
Terminal/normal dosyaya yönlendirilmiş stderr ve dosya log formatları değişmez.
`journalctl -p err` yalnız ERROR ve daha yüksek önemleri seçer; WARN kayıtlarını seçmez.
Bu eşleme yalnız logger kayıtlarını kapsar; doğrudan `eprintln!`, CLI yardım/hata çıktısı
ve panic çıktıları journal'ın varsayılan önceliğinde kalabilir. Servis durumunu da izleyin.
Dağıtılmamış kaynak değişikliği, canlı journal düzelmesi kanıtı değildir.

## Temel teşhis komutları

```bash
# Servis ve yeniden başlatma sayacı
systemctl is-active shoes && systemctl show shoes -p NRestarts

# Dinleyici
ss -H -ltnp4 'sport = :443'

# Son hatalar (salt okunur)
journalctl -u shoes --since "-5 min" -o cat | tail -n 20

# Kaynak özeti (INFO açıksa 60 sn'de bir): connections/streams/quic/per_ip
journalctl -u shoes --since "-2 min" -o cat | grep -i resource | tail -n 3

# İsteğe bağlı operatör script'i (depoya dahil değil; mevcutsa, onaylı canlı incelemede)
bash server-monitor.sh 5 12
```

Bu komutlar canlı inceleme için ayrıca onay gerektirir. Günlükleri yalnız gizli yerel oturumda inceleyin;
ham journal çıktısını rapora taşımayın. `ss` çıktısı tek başına TCP kabulü veya tünel sağlığı kanıtı değildir.

## Günlük düzeyi (marker dosyaları)

Düzey şu öncelikle belirlenir:

1. `.shoes-trace` marker dosyası → TRACE
2. `.shoes-debug` marker dosyası → DEBUG
3. `RUST_LOG` ortam değişkeni (`hedef=düzey` sözdizimi desteklenir)
4. Varsayılan: yalnız ERROR

Marker dosyaları, çalışan ikilinin bulunduğu dizinde aranır.

**Release sınırı:** Normal release derlemesinde `log/release_max_level_info`
etkindir; DEBUG/TRACE log makroları derleme sırasında statik olarak elenir.
Marker dosyaları release ikilide bu düzeyleri geri getirmez.

**Tanı ve gizlilik:** Marker dosyaları yalnız geçici tanı içindir; kalıcı açık
bırakılmaz. Ham günlük ve kimlik bilgisi raporlara eklenmez
(bkz. [guvenlik-ve-limitler.md](guvenlik-ve-limitler.md)).

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

- **RSS yüksek kaldı:** allocator rezervi, canlı veri veya sızıntı olabilir.
  Aynı stresin tekrarında sabit RSS tek başına sızıntıyı dışlamaz; allocator ve canlı tahsisleri ayrıca ölç.
- **Trim çalışmıyor gibi görünüyor:** koşullar — 5 dk aralık, RSS > 64 MB,
  aktif bağlantı < 128. Koşullardan biri sağlanmıyorsa trim yapılmaması beklenir;
  bağlantı sayısından trafik yükü veya sızıntı sonucu çıkarılmaz.
- **Trim kapsamı:** `malloc_trim` yalnız glibc belleğini hedefler; Linux CLI'ın
  Rust allocator'ı jemalloc'tır. `MADV_DONTNEED` görmek, jemalloc tamponlarının
  bu görevle temizlendiğini kanıtlamaz. Canlı izleme ayrıca sahip onayı gerektirir.
- **jemalloc kapsamı:** politika yalnız GNU/Linux CLI içindir
  (`src/allocator.rs`); kitaplık/FFI/diğer platform davranışı değişmez.
  Dolu `_RJEM_MALLOC_CONF` yerleşik varsayılanları geçersiz kılar.
  Uyarlanabilir RSS eşiğiyle açma/kapama yoktur.
- **Acil sıfırlama:** onaylı `systemctl restart shoes` (kısa kesinti).

## Reload ve yapılandırma

- Geçersiz yapılandırma reload'ı mevcut nesli **korur**; hata loglanır.
- Adres ailesi/wildcard kapsamı değişen geçişler (ör. `[::]:443` ↔ `0.0.0.0:443`)
  reddedilir; yeniden başlatma gerekir.
- Canlı birim `--no-reload` çalışır; bu yüzden dosya değişikliği kendiliğinden
  uygulanmaz — bilinçli tercih.
- `--dry-run` geçersiz yapılandırmada sıfır dışı kod döner; aday yapılandırmayı
  deploy öncesi doğrulamak için kullanılabilir. Canlı config `--dry-run`'a verilmez.

## Üretim profili redleri

`--production-profile` şu durumlarda yapılandırmayı reddeder:

- Dinleme adresi açık genel IPv4 değil veya port 443 değil.
- `transport: udp` kullanılmış.
- REALITY hedefi/SNI `whatsapp.net` değil (alt alan/başka kapak dahil).
- Opsiyonel VLESS fallback yapılandırılmış.
- Vision kapalı, VLESS UDP/XUDP kapalı veya birden fazla sunucu/ek TLS hedefi var.

## TUN

- TUN değişiklikleri canlıda uygulanamaz; `server_runtime` TUN canlı değişimini
  reddeder. Onaylı yeniden başlatma gerekir.

## Deploy sorunları

- **Hash uyuşmazlığı:** aktarım bozulmuş veya yanlış eser seçilmiş olabilir; kurulum yapmadan kaynağı doğrula.
- **Sağlık kontrolü başarısız:** otomatik rollback yoktur (2026-10-16 sahip kararı);
  operatör doğrulanmış `rollback.bin` ile elle geri döner ve sağlık kabulünü yineler.
- **SSH kesildi / sonuç bilinmiyor:** tekrar deploy etmeden canlı binary hash'i ve son bakım kaydını karşılaştır.
  Elle kurtarma da SSH/güç kaybı ve SIGKILL altında garanti değildir.
- **Unit/config hash'i değişti:** beklenmedik; deploy durdurulmalı ve incelenmeli.
- **NRestarts > 0:** beklenmedik yeniden başlatma; journal ve kaynak durumuna bak.

Esas adımlar ve süre sınırları [operasyon rehberindedir](operasyon.md).
Dağıtım yalnız binary değiştirir; unit/config oluşturulmaz veya düzeltilmez.
Yedek temizliği her zaman ayrı onaylı bakım işlemidir.

## CPU yükselmesi

- Tarayıcı/port taraması dalgası: `expected client handshake` hatası sayısı artar;
  kabul reddi freni (100 ms) bütçe doluyken CPU yakmayı önler.
- Gerçek yük: eşzamanlı bağlantı ve aktarım hacmine bak (server-monitor.sh).
- Sıcak sembol analizi için `perf` gerekir; ölçüm dışı dönemlerde kurulu tutulmaz.

## Kayıtlar nerede?

- Tarihsel inceleme notları — Git geçmişi (`git log --all -- report.md`).
- `SERVER-FEATURE-DECISIONS.md` — özellik kararları (depoda).
- `~/.local/state/shoes-live/` — canlı ölçüm, izleme script'leri, karar kayıtları.
- `~/.pi/agent/server-ssh.json` — bağlantı ayrıntıları (yerel).

## İlgili belgeler

- Operasyon: [operasyon.md](operasyon.md)
- Güvenlik sınırları: [guvenlik-ve-limitler.md](guvenlik-ve-limitler.md)
