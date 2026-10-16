# Güvenlik ve Doğrulama Limitleri

Bu belge, neyin **kanıtlandığını** ve neyin **kanıtlanmadığını** açıkça ayırır.
Hiçbir kısa test, hız kazancı veya tam güvenlik kanıtı yerine geçmez.

## Sağlanan korumalar

- **Fail-closed kapak:** REALITY doğrulaması başarısızsa trafik kapağa
  (`whatsapp.net`) iletilir; kapak başarısızsa bağlantı durur.
- **Kimlik doğrulama:** UUID yalnız bağlantı kurulumunda doğrulanır; veri
  yolunda kullanıcı araması yoktur.
- **Kaynak tavanları:** genel bağlantı/akış/QUIC bütçeleri isteğe bağlıdır;
  varsayılanları sınırsızdır. H2MUX 4096, UDP 2048 ve XUDP 16 384 tavanları
  ilgili bağlantı/akış başınadır; toplam süreç belleğini sınırlamaz.
- **Kabul reddi freni:** bütçe doluyken accept+close döngüsü engellenir.
- **Kapak hız sınırı (opsiyonel, varsayılan kapalı):** yalnız yetkisiz fallback
  trafiğini sınırlar; doğrulanmış kullanıcı aktarımı etkilenmez.
- **Dosya günlüğü satır bütünlüğü:** kontrol karakterleri ve Unicode satır
  ayırıcıları etkisizleştirilir; mesaj yeni günlük satırı veya terminal kontrolü ekleyemez.
  Bu koruma gizli bilgileri otomatik olarak ayıklamaz.
- **Vision durum günlüğü:** unpadder'ın debug gösterimi UUID veya biriktirilmiş
  paket baytları yerine durum adı ve tampon uzunluğu gösterir.
- **El sıkışma günlüğü:** tespit edilen REALITY/VLESS/XUDP ham bayt ve
  doğrulama etiketi dökümleri kaldırıldı; aşama ve uzunluk bilgileri korunur.
  Hedef adresleri gibi trafik metaverileri hâlâ loglanabilir; günlükleri gizli tutun.

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
- **Kapanış süresi sınırlı:** Unix CLI, SIGTERM/SIGINT sonrası dinleyicileri durdurur
  ve bağlantıları en fazla `min(reload_grace_secs, 30)` saniye boşaltır.
  Süre aşımında kalan bağlantılar iptal edilir. SIGKILL ve çökmeler bu yolu çalıştırmaz.
- **Rust uygulama, yerel bağımlılıklar:** kripto için AWS-LC ve Linux CLI allocator'ı
  jemalloc yerel/C kod içerir. Mevcut sürüm, tamamen Rust bağımlılık zinciri iddiası taşımaz.

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

- CI politikası, ağ çıkışı engelli, ayrılmış ve geçici runner gerektirir.
  Workflow dosyaları tek başına gerçek ağ izolasyonunu kanıtlamaz.
- Runner'da SSH anahtarı veya üretim kimlik bilgisi tutulmaz.
- `--offline` bir bağımlılık politikasıdır; tek başına tam ağ sandbox'ı değildir.
- Eksik araç/önbellek işi başarısız kılar; indirme tetiklemez.

## İlgili belgeler

- Operasyon: [operasyon.md](operasyon.md)
- Kararlar: [kararlar.md](kararlar.md)
- Performans sınırları: [performans-bellek.md](performans-bellek.md)
