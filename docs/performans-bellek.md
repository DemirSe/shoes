# Performans ve Bellek Geçmişi

Bu belge, bu fork'ta yapılan performans ve bellek çalışmalarının özetini tutar.
Ayrıntılı inceleme kayıtları Git geçmişinde (`git log --all -- report.md`) ve
`~/.local/state/shoes-live/` altındaki yerel kayıtlarda bulunur.

## Çalışma turları

| Tur | Commit | İçerik |
|---|---|---|
| Perf turu 1 | `d20b59f` | REALITY `read_tls` tek kopya; UoT ön izleme kapasite koruması; XUDP `freeze`/`read_filled` |
| Perf turu 2 | `489f989` | XUDP tek klon; VLESS çerçeve tamponu saklama; REALITY parçalı kayıt `scratch` yeniden kullanımı |
| Uzun ufuk sertleştirme | `af917e1` | LH-1…LH-7 (aşağıda) |
| Bellek (heap trim) | `a8fbd0b`, `5beea50` | Periyodik koşullu `malloc_trim`; eşik 64 MB, bağlantı kapısı 128 |

### Uzun ufuk düzeltmeleri (LH)

| No | Düzeltme | Etki |
|---|---|---|
| LH-1 | UoT teslim yolunda `put_slice` kapasite koruması | Gizli panik giderildi; paket tüketilmeden reddedilir |
| LH-2 | UdpRouter ping zamanlayıcısı `Skip` | Duraklama sonrası tick birikmesi önlendi |
| LH-3 | Accept red yolunda 100 ms fren | Bütçe doluyken accept+close döngüsü CPU yakmaz |
| LH-4 | XUDP tel oturumu tavanı 16 384 + O(1) tahliye | Sınırsız büyüme engellendi |
| LH-5 | `SpliceState` yorum düzeltmesi | Davranış değişikliği yok |
| LH-6 | H2MUX akış tavanı (yapılandırma yoksa 4096) | Kaynak tüketimi sınırlandı |
| LH-7 | UDP hedef tavanı (yapılandırma yoksa 2048) | Kaynak tüketimi sınırlandı |

### Seçilerek uygulanan / reddedilenler

- Uygulandı: REALITY tek kopya, XUDP iyileştirmeleri, Vision `scratch`,
  tampon saklama, UoT kapasite koruması, yukarıdaki LH seti.
- Uygulanmadı (sahip kararı): FxHashMap dönüşümleri, `pending_write`,
  REALITY `drain`, `memmem::Finder` statik, coalesced `copy_within`,
  ClientHello 64 KB tampon, `copy_bidirectional_message` 64 KB politikası.
- Belgelendi: LH-8…LH-11 ve yukarıdaki ertelenenler
  (bkz. [kararlar.md](kararlar.md)).

## Yerel A/B ölçümleri

- LH-1 guard: 0,5 → 0,7 ns/paket (+0,2 ns; gürültü seviyesi).
- LH-4 oturum başlatma: 32–34 → 56–58 ns/yeni oturum (paket başına değil,
  oturum kurulumu başına).
- Heap trim (release bench): `malloc_trim(0)` **0,018 ms**; RSS **261 → 5 MB**.
  C mikrobench (300 MB heap): 15 ms; trim sonrası yeniden dokunma cezası ölçülemedi.
  Bu tarihsel sonuç, güncel jemalloc kullanan CLI'ın Rust tamponlarının temizlendiğini kanıtlamaz.
  Kütüphane ve CLI test hedefleri farklı global allocator kullanır.

## Canlı ölçümler (2026-10-13)

| Ölçüm | Sonuç |
|---|---|
| Stres (1000 bağlantı + QUIC + indirme) | Tepe ~615 eşzamanlı; sunucu CPU ~%9; Rust sıcak sembolü yok |
| Aktarım yolu | `splice` etkin (`__x64_sys_splice`); UDP `sendto` ile |
| Tünel hızı | ~4–5 MB/s (istemci tarafı sınırlı); VPS doğrudan ~190 MB/s |
| TTFB (gstatic, tünel) | ort. ~0,24–0,32 sn |
| Ping (VPS, mobil yol) | ort. ~31–38 ms; min ~20 ms |
| Sunucu CPU (izleme) | %0,1–2,8; backlog sürekli 0 |
| Heap trim canlı kanıtı | `perf trace`: tek patlamada **102 × MADV_DONTNEED** |

## Bellek olayı (özet)

- 615 eşzamanlı bağlantılık stres sonrası RSS ~272 MB'ta kaldı.
- İkinci aynı stres RSS'i artırmadı. Bu gözlem tek başına sızıntıyı dışlamaz
  veya yüksek RSS'in glibc'den kaynaklandığını kanıtlamaz.
- Mevcut mekanizma: `src/memory.rs` — 5 dk aralık; RSS > 64 MB **ve** bağlantı < 128 iken
  `spawn_blocking` içinde `libc::malloc_trim(0)`.
- Çağrı yalnız glibc'nin yönettiği serbest sayfaları hedefler; Linux CLI'ın
  jemalloc ile ayırdığı Rust belleğini temizlemez. Yerel bağımlılıklar glibc kullanabilir.
- Bağlantı sayısı düşük yük garantisi değildir; ölçülen maliyet her iş yüküne genellenemez.
- Canlı doğrulama: görev paniksiz çalıştı; eşik altında trim yapmadı (doğru);
  geçici demo eşiğiyle trim'in çalıştığı syscall düzeyinde kanıtlandı.
  `madvise` gözlemi tek başına hangi allocator'ın veya tamponların temizlendiğini göstermez.

## Güncel politika ve yerel ölçüm sınırları (2026-10-15)

- Politika (yerel, devreye alma dışı): GNU/Linux CLI jemalloc arka plan geri
  kazanımı; `dirty_decay_ms=60000`, `muzzy_decay_ms=0`, arka plan temizliği açık.
  Modül: `src/allocator.rs` (yalnız CLI).
  Dolu `_RJEM_MALLOC_CONF` verilirse yerleşik varsayılanlar atlanır.
  Uyarlanabilir RSS eşiğiyle açma/kapama uygulanmaz.
- Arka plan bozunumu kesin 60 sn bırakma süresi veya RSS üst sınırı değildir.
- Yerel ölçüm: 16 KiB, 4 işçi, 256 MiB patlama, 8 MiB tutulan.
  60 sn bozunum 3 ayrı işlemde 99–106 sn içinde RSS < 64 MiB verdi.
  30 sn: 51–54 sn (n=3); 300 sn: 523 sn (n=1); bozunumsuz: 600 sn sonunda 325 MiB.
- 60 yeni yerel çalıştırma geçti; 25 kısa yeniden kullanım ve 25 TLS geri döngü içerdi.
- Aralıklar geniştir; sıfır gerileme veya hız kazancı garantisi yoktur.
  VPS/Hiddify platform matrisi kanıtı yoktur.
- `src/memory.rs` 5 dk/RSS 64 MiB/bağlantı < 128 bakımı glibc'ye özeldir.
  Bu çağrı jemalloc belleğini temizlemez; tamamlayıcı bakım olarak kalır.
- Yerel uygulama doğrulaması: gerçek `src/allocator.rs` kurulumu, işçiler açıkken
  256 MiB tampon deneyinde RSS'i 329,7 → 56,9 MiB'a 105,4 sn içinde indirdi;
  elde tutulan 8 MiB verinin bütünlüğü korundu. Bu testin 180 sn bütçesi genel bir süre garantisi değildir.
- Eski ve yeni release ikilileriyle beşer yerel TLS/VLESS koşusunda aktarım
  aralıkları örtüştü; hız kazancı veya sıfır gerileme kanıtı olarak yorumlanmadı.

## Ölçüm dersleri

- `ss -un` bağlanmamış UDP soketlerini gizler; `ss -tulpn` kullanılmalı.
- `rchar` soket okumalarını saymaz; indirme ölçümünde `bytes_sent`/eth0 sayılır.
- Uzun SSH oturumlarında `ServerAliveInterval` gerekir (hotspot kopması asılı kalır).
- Mobil hotspot istemcisi yüksek eşzamanlılığı sınırlayabilir; tepe eşzamanlılık
  ölçülmeden bellek davranışı yorumlanmamalı.
- Kısa testler hız kazancı veya tam güvenlik kanıtı **değildir**.

## İlgili belgeler

- Kararlar: [kararlar.md](kararlar.md)
- Güvenlik sınırları: [guvenlik-ve-limitler.md](guvenlik-ve-limitler.md)
