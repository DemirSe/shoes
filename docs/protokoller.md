# Protokol Akışları

Bu belge, sunucudan geçen trafiğin adım adım nasıl işlendiğini anlatır.
Wire biçimleri stok Xray/Hiddify uyumludur; bu fork onları değiştirmez.

## TCP bağlantı kurulumu (ana yol)

```
istemci                sunucu (shoes)                 hedef
  │  TCP SYN → 443           │                          │
  │  ClientHello (SNI=whatsapp.net)                       │
  │ ────────────────────────► │                          │
  │                     REALITY kimlik doğrulama          │
  │                     (başarısız → kapağa ilet)         │
  │ ◄──── ServerHello + sertifika akışı ────              │
  │  VLESS istek başlığı (UUID, komut, hedef)             │
  │ ────────────────────────► │  UUID doğrulama (tek kez)│
  │                           │ ──── hedef bağlantı ────► │
  │ ◄──── VLESS yanıt başlığı ┤                          │
  │  Vision dolgulu veri ⇄    │  (uygunsa splice)   ⇄    │
```

1. **TCP kabul** (`tcp_server.rs`): bağlantı bütçesinden izin alınır; limit
   doluysa 100 ms frenle reddedilir.
2. **REALITY** (`reality/`): TLS 1.3 el sıkışması taklit edilir; istemci kimliği
   X25519 + short_id + ClientHello alanlarıyla doğrulanır.
   - Doğrulanırsa sunucu tarafı TLS 1.3 anahtarları türetilir.
   - **Doğrulanmazsa** trafik aynı kapağa (`whatsapp.net`) iletilir; kapak
     başarısızsa bağlantı durur (fail-closed).
   - Desteklenen şifreleme paketleri: AES-128-GCM, AES-256-GCM, ChaCha20-Poly1305
     (`reality_cipher_suite.rs`). Varsayılan ayar değiştirilmedi.
   - Opsiyonel **kapak hız sınırı** (`fallback_rate_limit.rs`) koda eklendi;
     varsayılan **kapalı**; yalnız yetkisiz fallback trafiğini sınırlar,
     doğrulanmış kullanıcı aktarımı etkilenmez.
3. **VLESS** (`vless/`): istek başlığı UUID, komut (TCP/UDP) ve hedef adresini
   taşır. UUID doğrulaması **yalnız bağlantı kurulumunda** yapılır; veri yolunda
   kullanıcı araması yoktur. Sunucu tek `user_id` veya liste (`user_ids` takma
   adı) kabul eder; Vision yanıtı doğrulanan UUID'yi kullanır.
4. **Vision** (`vless/vision_*.rs`): TLS-in-TLS trafiği için dolgu (pad) ve
   çözme (unpad) filtreleri; okuma ve yazma geçitleri bağımsızdır.
5. **Hedef bağlantı**: doğrudan çıkış, IPv4 ve IPv6 desteklenir.
6. **Aktarım**: çift yönlü kopya. İki uç da ham TCP soketi sunduğunda ve ara
   bellekte bekleyen veri yoksa **`splice(2)`** hızlı yolu kullanılabilir
   (`linux_splice.rs`); aksi halde arabellekli yol sürer. Yerel soket tamponu
   32 KiB altındaysa splice kullanılmaz.

## UDP taşıma: XUDP

- UDP trafiği **TCP tüneli içinde** taşınır; sunucuda UDP dinleyicisi yoktur.
- XUDP çerçeveleri (`xudp/frame.rs`) hedef adres + uzunluk + veri taşır.
- Çok hedefli akışlar `routing/udp_router.rs` ile yönetilir:
  - Oturum tablosu: `IndexMap` + FxHasher, sabit yineleme sırası.
  - Ayrı gönderim/alım tampon havuzları; "zero-copy kuyruk" (doğrudan havuza oku).
  - `DelayQueue` ile oturum sonlanması O(1).
  - Akış başına hedef tavanı: yapılandırma yoksa **2048**.
- XUDP tel oturumu tavanı **16 384**; nesil-damgalı O(1) amortize tahliye.
- XUDP hedef çözümlemesi başarısızsa veya zaman aşımına uğrarsa yalnız ilgili
  datagram düşürülür; paylaşılan bağlantı ve mevcut route'lar kapatılmaz.
  Çözülemeyen hedef yeni route bütçesi tüketmez; sonraki datagram aynı oturumda yeniden deneyebilir.
- DNS beklemesi hâlâ sıralıdır; bekleme sırasında diğer paketler gecikebilir.
  Bu değişiklik paralel DNS veya yeni negatif cache politikası eklemez.
  Çok sayıda anlık çözümleme hatasında görev çalışma zamanına sıra bırakır.
- `End|ERROR` yalnız ilgili tel oturumunu kapatır. Diğer ERROR çerçeveleri,
  bozuk metadata ve taşıma hataları için önceki hata davranışı korunur.

## UDP-over-TCP (UoT) ve h2mux `packet_addr`

- `uot/` modülü SagerNet UDP-over-TCP v1 çerçevelemesini uygular:
  `[SOCKS5 adres][uzunluk:u16be][veri]`.
- Aynı biçim h2mux `packet_addr` akışlarında kullanılır.
- Teslim yolunda kapasite koruması vardır: küçük tampon, paketi tüketmeden
  `InvalidData` ile reddeder (gizli panik yok).

## H2MUX

- HTTP/2 tabanlı çoğullama (`h2mux/`); istemci zincirlerinde ve `packet_addr`
  akışlarında kullanılır.
- Oturum başına akış tavanı: yapılandırma yoksa **4096** (güvenlik tavanı).
- Etkinlik izleyicileri ve dolgu desteği vardır.

## DNS

- `dns/` + `resolver.rs`: DNS-over-QUIC (quinn) proxy çalışma zamanı.
- `quic_endpoint.rs`: quinn için `AsyncUdpSocket` uyarlaması; QUIC bağlantıları
  bellek bütçesinden izin alır (`quic_reserved_bytes`).
- Bilinen politika: **negatif DNS sonuçları cache'lenmez** (sahip kararı:
  uygulanmadı; bkz. [kararlar.md](kararlar.md)).

## Splice hızlı yolu (özet kurallar)

- Yön başına bir kez, yalnız okuma sorgusuyla karar verilir.
- Vision okuma ve yazma geçitleri bağımsız değerlendirilir.
- Dolgu modu ve sıradan TLS hiçbir zaman uygun değildir.
- Kuyruktaki ön veri, şifreli kuyruk ve boşaltma bariyeri önce boşalır.
- Splice reddedilirse veya boru ayrılamazsa arabellekli yol sürer.
- Ölçüm yereldir; sunucu/mobil/tüm tünel hız iddiası değildir.

## İlgili belgeler

- Yapılandırma karşılıkları: [konfigurasyon.md](konfigurasyon.md)
- Karar gerekçeleri: [kararlar.md](kararlar.md), [SERVER-FEATURE-DECISIONS.md](../SERVER-FEATURE-DECISIONS.md)
