# Kararlar ve Kısıtlar

Bu belge, sahibin verdiği kararları ve gerekçelerini özetler.
Tam karar kaydı: [SERVER-FEATURE-DECISIONS.md](../SERVER-FEATURE-DECISIONS.md).

## Değişmeyen kısıtlar

- Fork sunucuya hizmet eder; **stok istemci wire uyumluluğu** korunur.
- VLESS, REALITY, Vision, TCP, UDP ve XUDP korunur.
- Doğrudan çıkış IPv4 ve IPv6 destekler.
- Kaldırılanlar geri eklenmez: VMess, Trojan, Shadowsocks/ShadowTLS, HTTP/SOCKS.
- Dinleme: yalnız açık genel IPv4 TCP 443. **UDP 443 dinleyicisi asla eklenmez.**
- REALITY hedefi ve SNI tam olarak `whatsapp.net`; başka kapak/alt alan yok.
- Kapak başarısızsa bağlantı durur (fail-closed).
- Yetkisiz trafik aynı kapağa iletilebilir.
- Opsiyonel VLESS fallback yapılandırılmaz.
- Stok Hiddify uyumu: Android, Windows, Linux.
- TUN açık kalır; sahip bypass kuralı istemez.
- Tek paylaşılan UUID en fazla on cihaza hizmet eder; otomatik cihaz sayımı yok.
- `AGENTS.md` bilinçli olarak takipsizdir; sahip onayı olmadan commit edilmez.
- Bağımlılık ekleme veya `Cargo.lock` değişikliği sahip onayı ister.

## Özellik kararları (özet tablo)

| No | Özellik | Karar |
|---|---|---|
| 1 | Çoklu UUID (aynı REALITY profili) | **Gerekli.** Veri yolu değişmedi; doğrulama kurulumda. |
| 2 | Xray yönetim API'si | Gerekmez. |
| 3 | Kullanıcı bazlı trafik istatistiği | Gerekmez. |
| 4 | VLESS Encryption | Gerekmez (ölçülmüş kazanç yok). |
| 5 | REALITY ML-DSA | Gerekmez. |
| 6–9 | XHTTP / gRPC / WebSocket / HTTPUpgrade | Gerekmez; mevcut TCP taşıyıcısı korunur. |
| 10 | mKCP | Gerekmez; UDP tünel içinde kalır. |
| 11 | Stok Xray TCP mux | Gerekmez; mevcut kurulum korunur. |
| 12–14 | GeoIP/GeoSite, Sniffing, Observatory | Gerekmez. |
| 15 | Reverse | Gerekmez. |
| 16 | REALITY kapak hız sınırı | Kod eklendi; **varsayılan kapalı**; yalnız yetkisiz fallback trafiğini sınırlar. |
| 17 | Linux kernel splice | **Gerekli.** Uygun yönlerde kullanılır; uygun değilse arabellekli yol. |
| 18–21 | VMess, Trojan, Shadowsocks, HTTP/SOCKS | Gerekmez; geri eklenmez. |
| 22 | Xray yapılandırma uyumluluğu | Gerekmez. |

## Altyapı ve dağıtım kararları

- **Üretim profili** 2026-10-12'de sahip onayıyla etkinleştirildi; kaldırmak yeni
  onay ister. Geri dönüş yedeği: `unit-profile-*` dizinindeki `shoes.service.before`.
- **CI çevrimdışı ve elle**; VPS'e asla bağlanmaz.
- **APT elle**, otomatik yeniden başlatma kapalı.
- Elle deploy = release derleme → sahneleme (`rollback.bin` korunur) → atomik
  `install` + `mv` → restart → sınırlı sağlık gözlemi; hata durumunda elle geri dönüş.
- 2026-10-15: Yerel deploy script'i hazırlandı; varsayılanı ağsız plandı.
- 2026-10-16: Gereksiz bakım yükü nedeniyle yerel deploy script'i ve ona özel çevrimdışı
  testleri sahip kararıyla kaldırıldı. Otomatik rollback iddiası yoktur; canlı çalıştırma
  ayrıca onay ister.
- Dağıtım bakım yedeklerini otomatik silmez. Temizlik ayrı onaylı işlemdir;
  çalışan sürüm kaydı, kullanılabilir rollback binary'si ve gerekli unit yedeği korunur.
- Sinyal yönetimi (SIGTERM/SIGINT), Unix CLI'da libc `sigwait` ile uygulandı;
  yeni bağımlılık yok. Bağlantı boşaltma süresi en fazla `min(reload_grace_secs, 30)` saniyedir.

Güncel prosedürün esas kaynağı [operasyon.md](operasyon.md) dosyasıdır.
Tarihsel raporlar ve yerel test sonuçları, yeni kodun canlıya dağıtıldığını göstermez.

## Ertelenen ve reddedilen işler

| Konu | Durum / gerekçe |
|---|---|
| Otomatik artık Unix soket temizliği | Atlandı. Sahiplik kanıtı yetersiz; yarış riski var. |
| İçerik değişmediğinde reload atlama | Atlandı; dış PEM ve yan etkiler ek durum takibi ister; ölçülmüş ihtiyaç yok. |
| ClientHello 64 KB tampon | Atlanmış (parser sözleşmesi/test değişikliği; belirsiz kazanç). |
| `copy_bidirectional_message` 64 KB politikası | Ertelendi ("sonraya sakla"). |
| Negatif DNS cache | Belgelendi, uygulanmadı (sahip kararı). |
| Splice idle FD bırakma | Repro var; perf ölçümü ister; uygulanmadı. |
| REALITY KeyUpdate (post-handshake) | Belgelendi. Stok istemciler KeyUpdate göndermiyor; gerçek çözüm minimal değil. |
| h2mux küçük desenkronlar | Belgelendi; düşük/erişilemez etki. |

## Refute edilen adaylar

- Vision unpadder kısmi UUID düşürmesi (üretimde çağrı yolu yok).
- Sıfır kapasiteli `ReadBuf` → EOF karışıklığı (çağıranlar boş tampon üretmiyor).
- REALITY kapak 5 sn deadline (fail-closed tasarım; sahip kısıtı).
- Reload drenaj birikimi (`--no-reload` ile erişilemez).

## İlgili belgeler

- Performans/bellek geçmişi: [performans-bellek.md](performans-bellek.md)
- Güvenlik ve limitler: [guvenlik-ve-limitler.md](guvenlik-ve-limitler.md)
