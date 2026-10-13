# Performans ve Bellek Geçmişi

Bu belge, bu fork'ta yapılan performans ve bellek çalışmalarının özetini tutar.
Ayrıntılı inceleme kayıtları: [report.md](../report.md) ve
`~/.local/state/shoes-live/` altındaki yerel kayıtlar.

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
- İkinci aynı stres RSS'i artırmadı → **sızıntı değil, glibc arena rezervi**.
- Çözüm: `src/memory.rs` — 5 dk aralık; RSS > 64 MB **ve** bağlantı < 128 iken
  `spawn_blocking` içinde `libc::malloc_trim(0)`.
- Sıcak yol etkisi yok; wire/stok istemci etkisi yok.
- Canlı doğrulama: görev paniksiz çalıştı; eşik altında trim yapmadı (doğru);
  geçici demo eşiğiyle trim'in çalıştığı syscall düzeyinde kanıtlandı.

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
