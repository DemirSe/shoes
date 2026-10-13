# Mimari ve Kod Haritası

## Çalışma zamanı akışı

### Başlangıç

1. **`src/main.rs`** — CLI ayrıştırma:
   - `--threads`, `--dry-run`, `--no-reload`, `--production-profile`
   - Alt komutlar: `generate-reality-keypair`, `generate-vless-user-id`
   - Logging kurulumu; çok iş parçacıklı tokio runtime; `server_runtime::run` çağrısı.
2. **`src/server_runtime.rs`** — sıralı hazırlık:
   1. Yapılandırmayı yükle (`config::load`).
   2. Genel doğrulama + (bayrakla) üretim profili doğrulaması.
   3. `--dry-run` ise yazdır ve çık (geçersizse sıfır dışı kod).
   4. `PreparedGeneration::prepare` — **tüm dinleyiciler önce bağlanır**;
      adres değişmemişse canlı soketler yeniden kullanılır.
   5. `resources::configure` — genel limitler uygulanır.
   6. `ResourceReporter::start` + `memory::spawn_heap_trim_task`.
   7. `prepared.start()` — kabul döngüleri başlar.
   8. Ana `select!` döngüsü: dinleyici görevleri ve reload bildirimi.

### Reload (sıcak yenileme)

- Dosya izleyici (`start_notify_thread`) yapılandırma dizinini **ve** symlink'in
  kanonik hedefini izler.
- Değişiklikte yeni nesil hazırlanır ve bağlanır; **başarılıysa** eskiler durur,
  yeni nesil devralır. Bağlama başarısızsa veya yapılandırma geçersizse
  **mevcut nesil korunur**.
- Adres ailesi veya wildcard kapsamı değiştiren geçişler (ör. `[::]:443` ↔
  `0.0.0.0:443`) eski soket açıkken bağlanamaz; reload reddedilir, yeniden
  başlatma gerekir. Canlı birim `--no-reload` çalıştığı için bu yol kullanılmaz.

### Kapanış

- `--no-reload` canlıda etkin. Sinyal yönetimi (SIGTERM/SIGINT) **ertelendi**:
  tokio `signal` özelliği `signal-hook-registry` bağımlılığını gerektirir ve bu
  bağımlılık `Cargo.lock` içinde yoktur (kilit değişikliği sahip onayı ister).
- Kabul edilmiş bağlantıların boşaltılması `ListenerTasks` bırakma (Drop)
  mekanizmasına bağlıdır.

## Modül haritası

| Modül | Görev |
|---|---|
| `src/main.rs` | CLI, logging, runtime kurulumu |
| `src/server_runtime.rs` | Nesil hazırlama/başlatma, reload döngüsü |
| `src/config/` | Yapılandırma tipleri, PEM, doğrulama, `production.rs`, sing-box dönüşümü |
| `src/tcp/` | TCP/Unix dinleyiciler, kabul, zincir kurucu, soket bağlayıcıları |
| `src/reality/` | REALITY sunucu/istemci, AEAD, kimlik doğrulama, sertifika, TLS 1.3 anahtarları, kapak hız sınırı |
| `src/vless/` | VLESS sunucu/istemci, Vision dolgu/çözme, akış çerçeveleme |
| `src/xudp/` | UDP'nin TCP üzerinde çoğullaması (çerçeve + mesaj akışı) |
| `src/uot/` | SagerNet UDP-over-TCP v1 çerçevelemesi (h2mux `packet_addr` ile ortak) |
| `src/h2mux/` | HTTP/2 çoğullama (istemci zincirleri ve `packet_addr` akışları) |
| `src/routing/udp_router.rs` | Çok hedefli UDP yönlendirme, oturum tablosu, tampon havuzları |
| `src/tun/` | TUN cihazı, smoltcp TCP yığını, UDP yöneticisi, uyanma (wake) |
| `src/dns/` + `src/resolver.rs` | Çözümleyici; DNS-over-QUIC proxy çalışma zamanı (quinn) |
| `src/quic_endpoint.rs` | quinn `AsyncUdpSocket` uyarlaması + QUIC bellek izni |
| `src/linux_splice.rs` | Uygun ham TCP yönlerde çekirdek `splice(2)` hızlı yolu |
| `src/copy_bidirectional*.rs` | Çift yönlü aktarım (düz ve mesaj çerçeveli) |
| `src/client_proxy_chain.rs` | Çok atlamalı istemci zincirleri, havuz/round-robin |
| `src/resources.rs` | Bağlantı/akış/QUIC bütçeleri, limitler, per-IP ret sayaçları |
| `src/memory.rs` | Periyodik, koşullu glibc heap trim |
| `src/ffi/` | Android/iOS için FFI katmanı |
| `src/logging.rs` | Günlük seviyeleri ve hedefler |

## Kaynak bütçeleri ve güvenlik tavanları

- **Genel limitler** (`resources.rs`): eşzamanlı bağlantı, akış ve QUIC tampon
  baytı bütçeleri; aşılırsa bağlantı reddedilir (per-IP ret sayaçları izlenir).
- **Kabul reddinde 100 ms fren** (`tcp_server.rs`): bütçe doluyken accept+close
  döngüsü CPU yakmaz.
- **Güvenlik tavanları** — yapılandırma sınır vermezse kod varsayılanı uygular;
  açık yapılandırma her zaman kazanır:
  - H2MUX: bağlantı başına **4096** akış.
  - UDP yönlendirici: akış başına **2048** hedef.
  - XUDP: **16 384** tel oturumu; nesil-damgalı O(1) amortize tahliye.
- **UDP yönlendirici**: `IndexMap` + FxHasher; ayrı gönderim/alım tampon havuzları
  (açlık önleme); `DelayQueue` ile O(1) oturum sonlanması; ping zamanlayıcısı
  `MissedTickBehavior::Skip` (duraklama sonrası tick birikmez).

## Bellek yönetimi

- Bağlantı patlaması sonrası glibc serbest sayfaları tutabilir (RSS yüksek kalır).
- `src/memory.rs`: her **5 dakikada** bir kontrol; yalnızca
  **RSS > 64 MB** ve **aktif bağlantı < 128** iken, `spawn_blocking` içinde
  `libc::malloc_trim(0)` çağrılır (Linux + glibc koşulu).
- Sıcak yola etkisi yoktur; trim yalnız boşta çalışır. Ölçümler:
  [performans-bellek.md](performans-bellek.md).

## İş parçacığı ve görev yapısı

- Çok iş parçacıklı tokio runtime; dinleyici başına kabul döngüsü ayrı görev.
- Bağlantı başına görev: el sıkışma → protokol → aktarım.
- `ResourceReporter`: 60 sn aralıkla kaynak özeti loglar.
- Heap trim görevi: 5 dk aralık, koşullu; işi blocking havuza devreder.
- `splice` yolu tokio `Interest` ile asenkron sürülür; boru FD'leri
  `linux_splice.rs` sahipliğindedir.

## İlgili belgeler

- Protokol adımları: [protokoller.md](protokoller.md)
- Operasyon: [operasyon.md](operasyon.md)
- Performans/bellek geçmişi: [performans-bellek.md](performans-bellek.md)
