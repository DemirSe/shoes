# Genel Bakış ve Uçtan Uca Topoloji

## Bu proje nedir?

Bu depo, `shoes` 0.3.2'nin **sunucu tarafı için özelleştirilmiş bir fork'udur**.
Tek bir sansür dirençli profil sunar:

- **VLESS + REALITY + Vision**, açık genel IPv4 TCP 443 üzerinde.
- UDP trafiği (XUDP) aynı TCP tünelinin içinde taşınır; ayrı UDP dinleyicisi yoktur.
- Stok Xray/Hiddify uyumluluğu gereksinimdir (Android, Windows, Linux);
  tamamlanmış platform doğrulaması değildir.

Fork'un amacı yeni protokol eklemek değil; mevcut profili **ölçülmüş biçimde
sağlamlaştırmak, kaynak tüketimini sınırlamak ve uzun ömürlü çalışmayı
korumaktır**. Kullanılmayan protokoller kaldırılmıştır ve geri eklenmez.

## Sahip kısıtları (özet)

Tam liste [kararlar.md](kararlar.md) içinde. Özet:

- Stok istemci wire uyumluluğu korunur (Xray/Hiddify).
- REALITY hedefi ve SNI yalnız `whatsapp.net`; başka kapak yok.
- Kapak başarısızsa bağlantı durur (fail-closed).
- Dinleme yalnız açık genel IPv4 TCP 443; UDP 443 dinleyicisi asla eklenmez.
- Opsiyonel VLESS fallback yapılandırılmaz.
- TUN istemcide açık kalır; bypass kuralı istenmez.
- Tek paylaşılan UUID en fazla on cihaza hizmet eder (yönetsel sınır; otomatik sayım yok).
- Kaldırılanlar geri eklenmez: VMess, Trojan, Shadowsocks/ShadowTLS, HTTP/SOCKS.

## Uçtan uca topoloji

```
┌──────────────┐   WiFi hotspot    ┌───────────────────┐    mobil veri (5G)   ┌──────────────────┐
│  Telefon     │ ────────────────► │  Laptop           │ ───────────────────► │  VPS (sunucu)    │
│  (Android)   │                   │  (Linux)          │                      │  shoes.service   │
│  mobil veri  │                   │  TUN istemci      │   REALITY/VLESS      │  TCP 443         │
│  hotspot     │                   │  + yönetim + ölçüm│   (whatsapp.net SNI) │  IPv4 dinleyici  │
└──────────────┘                   └───────────────────┘                      └────────┬─────────┘
        │                                                                              │
        │ Android Hiddify ile doğrudan bağlanabilir (uyumluluk hedefi)                 │ doğrudan çıkış
        └──────────────────────────────────────────────────────────────────────────►   │ (IPv4/IPv6)
                                                                                       ▼
                                                                                 Hedef sunucular
```

### Bileşenler

| Bileşen | Rol | Notlar |
|---|---|---|
| Telefon (Android) | Mobil veri + WiFi hotspot | Laptop'a internet sağlar; ayrıca Hiddify ile doğrudan istemci olabilir (uyumluluk hedefi). |
| Laptop (Linux) | TUN istemci + yönetim + ölçüm | TUN açık; yönetim SSH'i WiFi arayüzüne bağlanır. Ölçüm script'leri burada. |
| VPS (sunucu) | `shoes` servisi | Debian; tek dinleyici IPv4 TCP 443; `--production-profile` ile çalışır. |
| Kapak (cover) | `whatsapp.net` | REALITY hedefi + SNI. Yetkisiz trafik aynı kapağa iletilir. |

### Veri yolu (uçtan uca)

1. Laptop'taki uygulama → TUN arayüzü → istemci (Hiddify/sing-box ailesi).
2. İstemci REALITY el sıkışması başlatır: SNI `whatsapp.net`, TCP 443.
3. Trafik telefon hotspot'u → mobil veri → internet → VPS genel IPv4:443 üzerinden gider.
4. Sunucu: TCP kabul → REALITY kimlik doğrulaması → VLESS UUID doğrulaması →
   Vision filtresi → hedefe **doğrudan çıkış** (IPv4/IPv6).
5. Dönüş yolu aynı tünelden tersine işler.
6. UDP: XUDP çerçeveleri aynı TCP bağlantısı içinde taşınır (UDP 443 dinleyicisi yok).
7. Kimliği doğrulanmayan trafik kapağa (`whatsapp.net`) iletilir; kapak başarısızsa
   bağlantı durur.

### Yönetim yolu

- Laptop → SSH → VPS. SSH, WiFi arayüzüne bağlanır
  (`-o BindInterface=wlp0s20f3`); yönetim yolu tünelden ayrılır.
  Bu, hotspot veya SSH kesintilerine karşı garanti sağlamaz.
- Uzun oturumlarda `ServerAliveInterval` kullanılır (hotspot kopmalarında asılı kalmayı önler).
- Ayrıntılı prosedürler: [operasyon.md](operasyon.md).

### Tasarım ilkeleri

- **Tek profil:** yeni taşıyıcı/protokol eklenmez; mevcut yol korunur.
- **Fail-closed kapak:** doğrulanmayan trafik kapağa gider; kapak yoksa bağlantı sürmez.
- **Sıcak yolda minimum iş:** kimlik doğrulama yalnız bağlantı kurulumunda yapılır;
  veri yolunda kullanıcı araması yoktur.
- **Kaynak bütçeleri:** bağlantı/akış/QUIC bellek bütçeleri ve güvenlik tavanları
  (bkz. [mimari.md](mimari.md)).
- **Ölçmeden iddia yok:** kısa testler hız veya tam güvenlik kanıtı sayılmaz
  (bkz. [guvenlik-ve-limitler.md](guvenlik-ve-limitler.md)).
