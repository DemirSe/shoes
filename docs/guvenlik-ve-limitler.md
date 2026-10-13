# Güvenlik ve Doğrulama Limitleri

Bu belge, neyin **kanıtlandığını** ve neyin **kanıtlanmadığını** açıkça ayırır.
Hiçbir kısa test, hız kazancı veya tam güvenlik kanıtı yerine geçmez.

## Sağlanan korumalar

- **Fail-closed kapak:** REALITY doğrulaması başarısızsa trafik kapağa
  (`whatsapp.net`) iletilir; kapak başarısızsa bağlantı durur.
- **Kimlik doğrulama:** UUID yalnız bağlantı kurulumunda doğrulanır; veri
  yolunda kullanıcı araması yoktur.
- **Kaynak tavanları:** bağlantı/akış/QUIC bütçeleri ve güvenlik tavanları
  (H2MUX 4096, UDP 2048, XUDP 16 384) sınırsız tüketimi engeller.
- **Kabul reddi freni:** bütçe doluyken accept+close döngüsü engellenir.
- **Kapak hız sınırı (opsiyonel, varsayılan kapalı):** yalnız yetkisiz fallback
  trafiğini sınırlar; doğrulanmış kullanıcı aktarımı etkilenmez.

## Kanıtlanmayan / sınırlı olan

- **"SNI dışında hiçbir şey görünmez" iddiası yoktur.** ISP; sunucu IP'sini,
  portu, trafik hacmini ve zamanlamayı görebilir. SNI seçimi, trafiğin WhatsApp
  trafiğinden ayırt edilemeyeceği garantisi değildir.
- **Tünel arızası sonrası koruma kanıtlanmadı.** Etkin tünelde sızıntı kontrolü,
  tünel çöktükten sonraki davranışı kanıtlamaz.
- **No-bypass birebir sağlanmıyor:** laptop istemcisinde özel IP'ler için
  doğrudan kural vardır; literal "bypass yok" koşulunu karşılamaz. Sahip kapsamlı
  bir istek olmadan istemci yönlendirmesi değiştirilmez.
- **Hiddify platform matrisi yeniden sınanmadı.** Android/Windows/Linux için
  sürüm bazlı uyumluluk matrisi yoktur; derleme korumaları incelendi.
- **Kısa fonksiyonel testler yeterli değildir.** Hız, dayanıklılık ve güvenlik
  iddiaları için karşılaştırmalı, uzun süreli ölçüm gerekir.
- **Kapak hız sınırı etkisi ölçülmedi**; bu yüzden varsayılan kapalıdır.
- **AES/ChaCha seçimi ölçülmedi**; mevcut şifreleme ayarı değiştirilmedi.
- **Sinyal yönetimi yok:** süreç SIGTERM/SIGINT'te düzgün kapanış garantisi
  vermez (ertelenmiş iş; bkz. [kararlar.md](kararlar.md)).

## Kimlik bilgisi politikası

- Kimlik bilgileri (UUID, REALITY anahtarı, short_id, parola), ham yapılandırma
  ve paket içerikleri **hiçbir raporda/commit'te yer almaz**.
- Canlı yapılandırma okunmaz; yalnız SHA-256 özeti kaydedilir.
- Bu depo herkese açıktır; belgelerde sunucu adresi yer tutucudur (`<VPS_IP>`).
- Yerel kayıtlar `~/.local/state/shoes-live/` ve `~/.pi/agent/server-ssh.json`
  altında tutulur; depoya girmez.

## Cihaz sınırı

- Paylaşılan UUID için **on cihaz** sınırı yönetsel bir kuraldır.
- Sunucuda otomatik cihaz sayımı yoktur ve sahip kararıyla eklenmemiştir.
- UUID sayısı gerçek cihaz sayısını göstermez.

## CI ve tedarik zinciri

- CI çevrimdışıdır; runner'lar ağ çıkışı engelli, ayrılmış ve geçicidir.
- Runner'da SSH anahtarı veya üretim kimlik bilgisi tutulmaz.
- `--offline` bir bağımlılık politikasıdır; tek başına tam ağ sandbox'ı değildir.
- Eksik araç/önbellek işi başarısız kılar; indirme tetiklemez.

## İlgili belgeler

- Operasyon: [operasyon.md](operasyon.md)
- Kararlar: [kararlar.md](kararlar.md)
- Performans sınırları: [performans-bellek.md](performans-bellek.md)
