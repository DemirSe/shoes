# Sunucu özellik kararları

## Kapsam

Bu dosya sahibin seçimlerini ve istediği kısa değerlendirmeleri kaydeder.
Yalnız 16 numaralı özellik koda eklendi.
Varsayılan olarak kapalıdır.
Canlı sunucu servisi ve istemci ayarları değişmedi.
Çoklu UUID ve splice uygulaması henüz başlamadı.
Çalışma sınırlarının kaynağı [proje talimatlarıdır](AGENTS.md).
Mevcut sunucu yönetimi yeterli kalır.
Bu kayıt dağıtım, staging, commit veya push izni vermez.

Sabit SNI ve cover kuralları proje talimatlarında kalır.
UDP/XUDP dışarıdaki TCP tüneli içinde taşınır.
Bu tasarım ISP’nin yalnız SNI gördüğü anlamına gelmez.
ISP sunucu IP’sini, portu, trafik hacmini ve zamanlamayı da görebilir.
SNI seçimi, trafiğin WhatsApp trafiğinden ayırt edilemeyeceği garantisi değildir.

## Seçimler

Numaralar önceki özellik listesiyle aynıdır.
Değerlendirme önerileri, sahibin henüz vermediği kararların yerine geçmez.

| No | Özellik | Sahibin kararı |
|---|---|---|
| 1 | Aynı REALITY profilinde çoklu UUID | Gerekli. Veri aktarım performansı korunmalı. |
| 2 | Xray yönetim API’si | Gerekmez. Mevcut yönetim yeterli. |
| 3 | Kullanıcı bazlı trafik istatistiği | Gerekmez. |
| 4 | VLESS Encryption | Gerekmez. Ek şifreleme katmanı eklenmeyecek. |
| 5 | REALITY ML-DSA | Gerekmez. |
| 6 | XHTTP | Gerekmez. Mevcut TCP taşıyıcısı korunacak. |
| 7 | gRPC taşıyıcısı | Gerekmez. Mevcut TCP taşıyıcısı korunacak. |
| 8 | WebSocket | Gerekmez. Mevcut TCP taşıyıcısı korunacak. |
| 9 | HTTPUpgrade | Gerekmez. Mevcut TCP taşıyıcısı korunacak. |
| 10 | mKCP | Gerekmez. UDP tünel içinde kalacak. |
| 11 | Stok Xray TCP mux | Gerekmez. Mevcut kurulum korunacak. |
| 12 | GeoIP/GeoSite | Gerekmez. |
| 13 | Sniffing | Gerekmez. |
| 14 | Observatory | Gerekmez. |
| 15 | Reverse | Gerekmez. |
| 16 | REALITY yetkisiz trafik hız sınırı | Uygulama tamamlandı. Varsayılan kapalı. Sunucuda etkinleştirmek için ayrı yapılandırma gerekir. |
| 17 | Linux kernel splice | Gerekli. Kazanç ölçülmeli ve mevcut kopyalama yolu korunmalı. |
| 18 | VMess | Gerekmez. Geri eklenmeyecek. |
| 19 | Trojan | Gerekmez. Geri eklenmeyecek. |
| 20 | Shadowsocks | Gerekmez. Geri eklenmeyecek. |
| 21 | HTTP/SOCKS | Gerekmez. Geri eklenmeyecek. |
| 22 | Xray yapılandırma uyumluluğu | Gerekmez. |

## Kısa değerlendirme

Karşılaştırma temeli stok Xray 26.3.27’dir.
Bu görev hiçbir seçenek için ölçülmüş hız kazancı üretmedi.
Aşağıdaki performans beklentileri, ölçüm sonucu değildir.

### 1. Çoklu UUID

Çoklu UUID desteği aynı REALITY profilinde birden fazla UUID kabul ettirecek.
UUID doğrulaması yalnız bağlantı kurulurken yapılmalı.
Her pakette kullanıcı araması yapılmamalı.
Mevcut tek UUID ayarı çalışmaya devam etmeli.
Aktarım yolunu değiştirmeden eklemek tasarım hedefidir.

Performansı koruduğumuzu ölçümlerle doğrulamamız gerekir.
UUID sayısı gerçek cihaz sayısını göstermez.
Bu özellik mevcut cihaz sınırını otomatik uygulatmaz.

### 4. VLESS Encryption ve şifreleme seçimi

VLESS Encryption, mevcut REALITY/TLS’den ayrı bir şifreleme katmanıdır.
Etkinleştirmek ek el sıkışması ve şifreleme işi getirebilir.
Bu profilde hız kazandırdığına dair karşılaştırmalı ölçüm yok.
Yalnız hız amacıyla eklemeyi önermiyorum.

Mevcut [REALITY kodu](src/reality/reality_cipher_suite.rs) AES-128-GCM, AES-256-GCM ve ChaCha20-Poly1305 destekler.
AES hızlandırması bulunan işlemcilerde AES-GCM değerlendirmeye adaydır.
AES hızlandırması bulunmayan işlemcilerde ChaCha20-Poly1305 değerlendirmeye adaydır.
İşlemci, uygulama ve iş yükü sonucu etkiler.
Bu VPS için hangi seçimin daha hızlı olduğunu ölçmedik.
Mevcut şifreleme ayarını değiştirmedik.

Stok Xray bu katmanla Vision kullanımını doğrudan reddetmez.
Ancak uyumluluk, ham kopyalama veya splice uygunluğu anlamına gelmez.
Her Hiddify sürümünde bu ek şifreleme desteğini varsayamayız.

### 5. REALITY ML-DSA

ML-DSA ek sertifika imzası ve istemci doğrulaması sağlar.
Amaç aktarım hızını artırmak değildir.
Ek imza işi ve sertifika verisi bağlantı kuruluşunu etkiler.
Her veri paketinde yeniden imza üretmez.
ML-KEM anahtar değişimiyle aynı özellik değildir.

İstemcinin doğrulama desteği ayrıca kontrol edilmeli.
Resmî belge, hedefin geçici sertifikası için 3500 bayttan büyük boyut ister.
Mevcut sabit cover’ın bu koşula uygunluğunu kontrol etmedik.

### 6–11. Taşıyıcılar ve TCP mux

Taşıyıcı değiştirmek istemcide eşleşen ayarlar ve destek gerektirir.
Hiçbiri bu sunucuda kanıtlanmış hız yükseltmesi değildir.

| No | Kısa açıklama | Performans ve uyumluluk |
|---|---|---|
| 6 | XHTTP, veriyi HTTP istekleri ve akışlarıyla taşır. | Ağ engellerine uyum amaçlıdır. Hız garantisi yok. Stok sing-box taşıyıcı listesinde bulunmaz. Hiddify desteği motor ve sürüme bağlıdır. |
| 7 | gRPC, HTTP/2 üzerinden veri akışı taşır. | HTTP çerçeveleri ve çoğullama ekler. Stok Xray REALITY ile destekler. İstemci derlemesinde destek gerekir. Yönetim API’sinden ayrı özelliktir. |
| 8 | WebSocket, HTTP yükseltmesinden sonra çerçeveli veri taşır. | Ara HTTP sunucularıyla kullanılabilir. Çerçeveleme işi ekler. Stok Xray 26.3.27 REALITY ile birlikte desteklemez. |
| 9 | HTTPUpgrade, HTTP yükseltmesinden sonra bayt akışı taşır. | WebSocket çerçevelerini kullanmaz. Bu tasarım farkı ölçülmüş hız kazancı değildir. Stok Xray 26.3.27 REALITY ile birlikte desteklemez. |
| 10 | mKCP, UDP üzerinde güvenilir akış taşır. | Bazı yollarda gecikme için daha fazla bant tüketir. UDP dinleyicisi gerekir. Mevcut TCP-only sınırıyla çelişir. Stok sing-box ve bu REALITY profili için uygun değildir. |
| 11 | TCP mux, bir TCP bağlantısında birden fazla mantıksal bağlantı taşır. | Bağlantı kuruluş maliyetini azaltabilir. Tek bağlantıdaki kayıp diğer akışları da bekletebilir. Büyük aktarımı yavaşlatabilir. UDP/XUDP ve H2MUX ile aynı özellik değildir. |

Stok Xray REALITY taşıyıcı seçenekleri RAW/TCP, XHTTP ve gRPC’dir.
Bu seçeneklerin her Vision birleşimini mevcut testler doğrulamaz.
İleri taşıyıcılarda Vision’ın ham kopyalama yolu her zaman kullanılamaz.

11 için önerim mevcut kurulumda ek TCP mux kullanmamaktır.
Sahip genellikle Türk Telekom 5G hattı kullanır.
Mobil yolda kayıp veya gecikme, ortak TCP mux bağlantısındaki birçok akışı bekletebilir.
Bu, operatöre özel ölçülmüş bir sorun değildir.
Türk Telekom 5G üzerinde karşılaştırmalı mux testi yapılmadı.
Sahip ek TCP mux kullanmamayı seçti.

### 15. Reverse

Özel ağdaki cihaz, erişilebilir sunucuya dışarı doğru bağlantı açar.
Sunucu bu bağlantı üzerinden özel ağdaki servise erişim sağlar.
Örnek kullanım, NAT arkasındaki ev servisine erişmektir.
İnternet çıkışını hızlandıran bir özellik değildir.
Mevcut kullanım için ihtiyaç gösterilmedi.

### 16. REALITY yetkisiz trafik hız sınırı

Bu özellik doğrulamayı geçemeyen bağlantıların cover’a iletilen trafiğini sınırlar.
Opsiyonel VLESS fallback ayarı değildir.
Sunucunun kullanıcı trafiğini hızlandırmaz.
Yetkisiz trafiğin kaynak tüketimini azaltabilir.
Kimliği doğrulanmış veri aktarımı sınırlayıcıya girmemeli.

Kapalı seçenek mevcut davranışı korumalı.
Kullanıcı istatistiği veya yönetim API’si eklenmemeli.
İç sayaçlar yalnız sınırlayıcının çalışması için kullanılabilir.

Resmî belge, belirgin hız sınırlarının trafik parmak izi oluşturabileceğini bildirir.
Bu nedenle seçenek varsayılan olarak kapalı kalmalı.
Etkisini ölçmeden agresif değer seçmemeliyiz.
Mevcut cover değişmemeli.

Yavaş boşaltma sınırlıdır. İlerleme varken bekleme uzar.
İlerleme durursa bekleme beş saniye sonra biter.
Toplam bekleme, ilk yarı kapatmadan sonra altmış saniyeyi geçmez.
Bu andan sonra yazılmayan baytlar kaybolur.
Yazma hatası bağlantıyı hemen bitirir ve kuyruktaki baytları kaybedebilir.
Bu sınırlar yalnız yetkisiz fallback trafiği içindir.
Yetkili aktarım bu kurallardan etkilenmez.

### 17. Linux kernel splice

Splice, uygun TCP soketleri arasında çekirdek üzerinden veri kopyalar.
Kullanıcı alanındaki kopyalama işini azaltabilir.
Kazanç işlemciye ve darboğaza bağlıdır.
UDP/XUDP için doğrudan hızlandırma değildir.

Vision yalnız uygun ham TCP aşamasında bu yolu kullanabilir.
Ara bellekteki veriler ve TLS durumu güvenli geçişi belirler.
Uygun olmayan bağlantılarda mevcut kopyalama yolu kalmalı.
Sahip splice desteğini eklemeyi seçti.
Performans kazancı yine ölçüm gerektirir.

### 18–21. Diğer proxy protokolleri

Bunlar mevcut VLESS profilinin yerine geçen ayrı seçeneklerdir.
Sunucuya kullanılmayan protokol eklemek mevcut bağlantıyı hızlandırmaz.

| No | Özellik | Kısa açıklama |
|---|---|---|
| 18 | VMess | Kimlik doğrulama ve kendi şifrelemesi bulunan proxy protokolüdür. Ek protokol işlemleri vardır. Mevcut profile karşı hız kazancı ölçülmedi. |
| 19 | Trojan | Parola ile kimlik doğrulayan proxy protokolüdür. Güvenli kullanımda TLS gibi koruma gerekir. Mevcut profilden hızlı olduğuna dair ölçüm yok. |
| 20 | Shadowsocks | TCP/UDP taşıyan, AEAD şifrelemesi kullanan ayrı proxy protokolüdür. Algoritma ve iş yükü performansı etkiler. Gerekli VLESS/REALITY profilinin karşılığı değildir. |
| 21 | HTTP/SOCKS | Uygulamaların proxy’ye bağlanma protokolleridir. Tek başlarına veri şifrelemezler. Mevcut TUN kullanımı için ihtiyaç gösterilmedi. |

## Uygulama kabul koşulları

- 1 için sentetik testler izin verilen ve verilmeyen UUID’leri kontrol etmeli.
- 1 için VLESS, REALITY, Vision, TCP, UDP ve XUDP testleri korunmalı.
- Vision yanıtı doğrulanan UUID’yi kullanmalı.
- Eşleşen önce/sonra ölçümleri aktarım hızını, işlemci kullanımını ve tahsis maliyetini kapsamalı.
- Anlamlı performans kaybı kabulü engeller.
- 16 için doğrulanmış kullanıcıların aktarımı etkilenmemeli.
- 17 için ara bellek, kısmi ilerleme, geri basınç, iptal, hata ve yarı kapatma davranışları korunmalı.

## Kanıt ve sınırlar

Yerel inceleme ve kamuya açık kaynak araştırması tamamlandı.
Yeni performans testi veya platform testi yapılmadı.
Android/Windows/Linux Hiddify sürümleri için ek özellik matrisi yok.
Mevcut işlev testleri, önerilen ek özelliklerin uyumluluğunu kanıtlamaz.

- [Kaynak araştırması](/home/demir/.pi/agent/sessions/--home-demir-pi-xray-optimized--/subagent-artifacts/outputs/1c7935bc-362b-4cf9-93a9-dda394b0af27/feature-performance-brief.md).
- [Bağımsız kanıt kontrolü](/home/demir/.pi/agent/sessions/--home-demir-pi-xray-optimized--/subagent-artifacts/outputs/1c7935bc-362b-4cf9-93a9-dda394b0af27/feature-performance-evidence-audit.md).
- [Stok Xray VLESS yapılandırması](https://github.com/XTLS/Xray-core/blob/v26.3.27/infra/conf/vless.go).
- [Stok Xray ham kopyalama ve splice kodu](https://github.com/XTLS/Xray-core/blob/v26.3.27/proxy/proxy.go).
- [Resmî REALITY seçenekleri](https://github.com/XTLS/Xray-docs-next/blob/main/docs/en/config/transports/reality.md).
- [Stok sing-box taşıyıcıları](https://sing-box.sagernet.org/configuration/shared/v2ray-transport/).

Etiketli kod sürümün davranışına kanıttır.
Etiketsiz resmî belgeler ileride değişebilir.
Araştırmanın AES/ChaCha seçim ayrıntısını bağımsız kontrol yeniden doğrulamadı.
Bu kayıt Xray’in otomatik algoritma seçiminden ölçülmüş kazanç sonucu çıkarmaz.
