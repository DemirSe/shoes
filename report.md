# Öneriler

Tarih: 2026-10-12

## Kapsam

Bu dosya, düzeltmelerin (B001–B005, G001–G002) ardından yapılan statik kod incelemesinin önerilerini içerir.
İnceleme yalnızca kod okumayla yapıldı; test, derleme veya canlı bağlantı kullanılmadı.
Aşağıdakiler hata raporu değildir; öncelik sıralı iyileştirme önerileridir.
Hiçbiri canlıya dağıtım, bakım veya ayar değişikliği izni vermez.

Düzeltmelerin durumu: uygulanmış ve önceki rapora göre doğrulanmış kabul edildi.
Kod incelemesinde düzeltmelerin genel yaklaşımı optimal bulundu.
Öneriler kalan sınırlar, uç durumlar ve isteğe bağlı sadeleştirmelerle ilgilidir.

## Öncelik özeti

| No | Öneri | Öncelik | Alan |
|---|---|---|---|
| Ö1 | Sinyal yönetimi ve düzgün kapanış | Orta-Yüksek | `src/main.rs`, `src/server_runtime.rs` |
| Ö2 | Unix soket artığı politikası | Orta | `src/tcp/tcp_server.rs`, doküman |
| Ö3 | Reload adres geçişi sınırını belgeleme | Düşük-Orta | `CONFIG.md` |
| Ö4 | Symlink'li yapılandırmada izleme | Düşük | `src/main.rs` |
| Ö5 | İçerik değişmediğinde reload turunu atlama | Düşük | `src/server_runtime.rs` |
| Ö6 | Kozmetik ve küçük tutarlılıklar | Düşük | `src/server_runtime.rs`, `src/tcp/tcp_server.rs` |
| Ö7 | `public_ipv4` sadeleştirmesi | İsteğe bağlı | `src/config/production.rs` |
| Ö8 | CI sertleştirme | Orta | `.github/workflows/*`, `.github/OFFLINE-CI.md` |
| Ö9 | Ölçüm ve kanıt notları | İsteğe bağlı | doğrulama süreci |

---

## Ö1 — Sinyal yönetimi ve düzgün kapanış

### Neden

Süreç SIGTERM veya SIGINT aldığında varsayılan davranış anında sonlanmadır.
Rust'ta destructor'lar çalışmaz.

- `OwnedUnixListener::Drop` çalışmadığı için Unix soket dosyası kalabilir.
- Açık bağlantılar `reload_grace_secs` beklemeden kesilir.
- `systemd` yeniden başlatmaları ve canlı bakım bu yolu sık kullanır.

Bu, `report.md` sınırlar bölümünde kayıtlı kalan yükümlülüğün teknik karşılığıdır.

### Nerede

- `src/server_runtime.rs:133-170` — ana `select!` döngüsü.
- `src/main.rs:276-290` — runtime kurulumu ve hata çıkışı.
- `Cargo.toml:88-96` — tokio özellikleri (`signal` yok).

### Önerilen yaklaşım

1. `Cargo.toml` içindeki tokio özelliklerine `signal` ekleyin.
   Sürüm değişmez; tokio zaten `Cargo.lock` içinde ve kaynak önbellekte olduğundan çevrimdışı derleme etkilenmez.
2. `server_runtime::run` içindeki `select!` döngüsüne üçüncü bir kol ekleyin:
   - Unix: `tokio::signal::unix::signal(SignalKind::terminate())` ve `ctrl_c`.
   - Diğer platformlar: `ctrl_c`.
3. Sinyal kolunda:
   - `stop(&mut tasks).await` ile kabul döngülerini durdurun.
   - `registry` düşerek Unix soket yollarını temizlesin.
   - `Ok(())` döndürün; süreç `0` koduyla çıksın.
4. Kabul edilmiş bağlantılar için yeni bir mekanizma gerekmez.
   `listener_tasks::ListenerTasks` Drop mekanizması zaten `reload_grace_secs` içinde boşaltıyor.

### Doğrulama önerisi

`tests/cases/integration/server_lifecycle.rs` içine test ekleyin:

- SIGTERM sonrası süreç hızlı ve sıfır kodla çıkar.
- Unix soket dinleyicisi kullanıldıysa dosya silinir.
- Grace süresi içindeki aktif aktarım tamamlanır.

### Notlar

- Davranış değişikliği dışarıdan görünür; canlıya dağıtım için sahip onayı gerekir.
- Windows'ta yalnız `ctrl_c` anlamlıdır; platform koşulları `#[cfg]` ile ayrılmalıdır.

---

## Ö2 — Unix soket artığı politikası

### Neden

Çökme veya SIGKILL sonrası kalan soket dosyası, sonraki başlangıçta `AddrInUse` ile durur.
Mevcut tasarım yabancı dosyaları asla silmediği için bu bilinçli bir güvenlik dengesidir.
Ancak işletim kolaylığı için kontrollü bir yol değerlidir.

### Nerede

- `src/tcp/tcp_server.rs:431-465` — `OwnedUnixListener` bind ve Drop.
- `src/socket_util.rs:238-256` — `new_unix_listener` doğrudan bind eder.

### Önerilen yaklaşım

1. Önce Ö1'i uygulayın; normal kapanışta temizlik zaten çözülür.
2. Kalan çökme senaryosu için isteğe bağlı bir `--replace-own-socket` bayrağı düşünün:
   - Yol `symlink_metadata` ile soket tipindeyse ve `UnixStream::connect` bağlantı reddi veriyorsa,
   - Dosya sahibi bu kullanıcıysa,
   - Yalnız bu bayrak verildiğinde silin ve hemen bind edin.
3. Bayrak yoksa mevcut güvenli davranış korunur; `AddrInUse` açık hata olarak raporlanır.
4. `CONFIG.md` içinde artık temizliği ve bayrağın kapsamı belgelenir.

### Risk

Canlı bir dinleyiciye ait soketin yanlışlıkla silinmesi, çalışan servisi etkileyebilir.
Bu yüzden bağlantı yoklama + sahiplik kontrolü + açık bayrak üçlüsü birlikte kullanılmalıdır.

---

## Ö3 — Reload adres geçişi sınırını belgeleme

### Neden

Yeni nesil, eski dinleyiciler durmadan bind edilir.
Bu tasarım bağlantı reddi penceresini önler ve çoğu geçişte sorunsuzdur.

Bazı geçişler eski soket açıkken mümkün değildir:

- `[::]:443` → `0.0.0.0:443` veya tersi.
- `socket_util::new_tcp_listener` `IPV6_V6ONLY` ayarlamaz (`src/socket_util.rs:205-236`).
- Varsayılan çift yığın davranışı nedeniyle v6 wildcard v4-mapped adresleri de kapsar.
- Bu durumda yeni bind çakışır, reload reddedilir ve mevcut nesil korunur.

Aynı ailedeki farklı adresler ve SO_REUSEADDR'lı wildcard + belirli adres çiftleri genelde birlikte yaşar.
Kesin davranış çekirdeğe bağlıdır; kod statik olarak garanti veremez.

### Önerilen yaklaşım

1. Kısa vadede `CONFIG.md` reload bölümüne bir paragraf ekleyin:
   - Aile veya wildcard kapsamı değiştiren geçişler reddedilirse yeniden başlatma gerekir.
2. Çakışmayı otomatik çözmek yalnızca gerçek ihtiyaç doğarsa düşünülmelidir.
   "Durdur, bind et, başarısızsa geri yükle" yolu karmaşıktır ve hata riski taşır.
3. Üretim profili sabit tek adres kullandığı için pratik etki düşüktür.

---

## Ö4 — Symlink'li yapılandırmada izleme

### Neden

`start_notify_thread` yapılandırma dosyasının üst dizinini izler (`src/main.rs:96-107`).
Dosya yolu bir symlink ise ve gerçek hedef başka dizindeyse, hedefteki düzenleme reload tetiklemez.

### Önerilen yaklaşım

- İzleme kurulmadan önce `std::fs::canonicalize` ile gerçek yolu hesaplayın ve o dizini izleyin.
- Atomik değişim senaryosunda symlink yeniden oluşturulursa `Create`/`Remove` olayları yine yakalar.
- `server_lifecycle.rs` içine symlink senaryosu için küçük bir test ekleyin.

### Notlar

- Öncelik düşüktür; tipik kurulumda yapılandırma düz dosyadır.
- Canonicalize başarısız olursa mevcut davranışa düşülmelidir.

---

## Ö5 — İçerik değişmediğinde reload turunu atlama

### Neden

Bazı düzenleyiciler ve `touch` benzeri işlemler içeriği değiştirmeden olay üretir.
Bu durumda tam nesil yenilemesi çalışır; zararsız ama gereksizdir.

### Önerilen yaklaşım

- Yüklenen ve doğrulanan yapılandırmanın içerik özetini (veya dosya bayt özetini) saklayın.
- Özet aynıysa prepare ve swap adımlarını atlayın; debug seviyesinde loglayın.
- Özet karşılaştırması `global_limits` dahil tüm doğrulanmış içeriği kapsamalıdır.

### Notlar

- Öncelik düşük; davranış doğruluğu bu turda zaten korunuyor.
- Yarış koşullarına dikkat: özet, başarılı swap sonrasında güncellenmelidir.

---

## Ö6 — Kozmetik ve küçük tutarlılıklar

1. `prepare_tcp_servers_inner` içindeki `println!("Starting ...")` (`src/tcp/tcp_server.rs:548`)
   reddedilen reload'larda da yazılır.
   Reload yolunda debug seviyesine indirilebilir veya prepare aşaması sessizleştirilebilir.
2. `server_runtime::stop` (`src/server_runtime.rs:105-110`) iptal ile paniği aynı mesajda toplar.
   `JoinError::is_panic()` ile ayrıştırmak logları netleştirir.
3. `src/server_runtime.rs:155-157` içindeki `async { ... }.await` sarmalayıcısı gereksizdir;
   doğrudan `await` zinciri yeterlidir.
4. `main.rs` içindeki `println!`/`eprintln!` karışımı logging modülüyle tutarlı hale getirilebilir.

Öncelik düşüktür; işlevsel etkisi yoktur.

---

## Ö7 — `public_ipv4` sadeleştirmesi

### Durum

`src/config/production.rs:12-24` içindeki elle yazılmış liste testlerle doğrulanmıştır.
CGNAT, benchmark, dokümantasyon ve ayrılmış aralıkları kapsar.

### Öneri

- İsteğe bağlı olarak std `Ipv4Addr::is_global()` (Rust 1.84+) ile karşılaştırın.
  Bazı özel blokların kapsamı iki yöntemde farklı olabilir; testler iki davranışı da sabitlemelidir.
- Ya da mevcut liste korunup başına kapsanan aralıkları açıklayan kısa bir yorum eklenebilir.

### Notlar

- Zorunlu değildir; mevcut sürüm üretim profilinin amacını karşılar.

---

## Ö8 — CI sertleştirme

### Neden

Dört workflow da `HEAD == GITHUB_SHA` kontrolü yapar (`test.yml`, `lint.yml`, `build.yml`, `mobile.yml`).
Bu kontrol çalışma ağacındaki takip edilen değişiklikleri görmez.
Runner'da uncommitted bir düzenleme işi geçirebilir.

### Önerilen yaklaşım

1. Kaynak doğrulama adımına takip edilen dosya temizliği kontrolü ekleyin:

   ```bash
   test "$(git rev-parse HEAD)" = "$GITHUB_SHA"
   test -z "$(git status --porcelain --untracked-files=no)"
   ```

2. `.github/OFFLINE-CI.md` içine runner ağ izolasyonu doğrulama kontrol listesi ekleyin:
   - Egress engelinin kanıtı (ör. ağ erişimi denemesi başarısız olmalı).
   - Docker dahil tüm çıkış yollarının kapalı olduğu kontrolü.
   - Loopback testlerinin çalıştığı doğrulaması.
3. `--offline` ve `CARGO_NET_OFFLINE` zaten doğru konumdadır; değişiklik gerekmez.
4. Mobil işlerde araç kontrolleri hem workflow'da hem betiktedir; tekrar zararsızdır.

### Notlar

- Runner kurulumu ve gerçek iş koşumu ayrı, onaylı bir çalışma gerektirir.
- VPS erişimi eklenmemelidir.

---

## Ö9 — Ölçüm ve kanıt notları

### Çoklu UUID

- Doğrulama yalnızca bağlantı kurulumunda yapılır; veri yolu değişmemiştir.
- Kurulum başına maliyet, liste uzunluğuyla doğrusal sabit zamanlı karşılaştırmadır.
- Tipik liste (1–10 UUID) için etki ihmal edilebilir düzeydedir.
- Yine de performans iddiası için isteğe bağlı bir mikrobenchmark veya en azından ölçümün yapılmadığını belirten not tutarlıdır.

### Reload

- Bağlantı kesintisi olmadığı integration testleriyle kapsanmaktadır.
- Canlı yük altında reload davranışı ayrı, onaylı bir doğrulama olarak planlanabilir.

### Not

Bu ölçümler hız kazancı veya tam güvenlik kanıtı yerine geçmez.

---

## Uygulama notları

- Tüm öneriler için değişiklik kapsamı küçük tutulmalıdır.
- `Cargo.toml` değişikliği yalnızca Ö1 için ve yalnızca özellik ekleme biçiminde önerilir.
- Bağımlılık sürümleri ve `Cargo.lock` değiştirilmemelidir.
- Canlıya dağıtım, staging, commit ve push için ayrı sahip onayı gerekir.
- Bu dosya yalnızca öneri kaydıdır; uygulama veya dağıtım izni vermez.

---

## Seçerek uygulama sonucu

Tarih: 2026-10-12

Sahibin sonraki “seçerek uygula” isteği doğrultusunda uygun öneriler yalnız yerel koda uygulandı.
Yukarıdaki öneri metni korundu; aşağıdaki bölüm güncel uygulama kararını ve doğrulamayı kaydeder.
Canlı sunucuya bağlanılmadı veya dağıtım yapılmadı.

### Kararlar

| Öneri | Karar | Gerekçe / sonuç |
|---|---|---|
| Ö1 | Ertelendi. | Tokio `signal` için gereken `signal-hook-registry`, mevcut `Cargo.lock` içinde yok. Kilit ve bağımlılıklar değiştirilmedi. |
| Ö2 | Otomatik silme bayrağı atlandı. | Aynı kullanıcıya ait olmak, soketin bu servise ait olduğunu kanıtlamaz. Yoklama ile silme arasında çalışan dinleyiciye zarar verebilecek yarış vardır. |
| Ö3 | Uygulandı. | Adres ailesi ve wildcard kapsamı değişimindeki bind çakışmaları `CONFIG.md` içinde belgelendi. Otomatik durdurup yeniden bind etme eklenmedi. |
| Ö4 | Uygulandı. | Hem yapılandırılan yol hem mevcut canonical hedef izleniyor. Başarılı reload izlemeyi yeniliyor; kanala ulaşmış bekleyen değişiklik korunuyor. |
| Ö5 | Atlandı. | İçerik önbelleği eklenmedi. Dış PEM kaynakları ve reload yan etkilerinin kapsamını doğru belirlemek için ek durum takibi gerekiyor. Ölçülmüş ihtiyaç yok. |
| Ö6 | Seçilerek uygulandı. | Hazırlık mesajı debug loguna taşındı. Watcher hatası loglanıyor. Görev paniği ve iptali ayrıldı. Gereksiz async sarmalayıcı kaldırıldı. |
| Ö7 | Yorum seçeneği uygulandı. | Muhafazakâr IPv4 filtresi korundu ve kapsamı açıklandı. `is_global()` kullanılmadı; mevcut toolchain kontrolü `E0658` verdi. |
| Ö8 | Uygulandı. | Altı CI işinde takip edilen kirli kaynak kontrolü eklendi. Runner ağ izolasyonu kontrol listesi belgelendi; ağ denemesi yapılmadı. |
| Ö9 | Ölçüm sınırı notu korundu. | Yeni benchmark yapılmadı. Küçük UUID listelerinde etkinin ihmal edilebilir olduğu, bu görevde ölçülmüş bir sonuç olarak kabul edilmedi. |

### Ö1 hakkında teknik düzeltme

Önerideki “Tokio kilitte bulunduğu için signal özelliği çevrimdışı derlemeyi etkilemez” varsayımı bu depo için yeterli değildir.
Unix signal özelliği ek bir bağımlılık gerektirir; bu bağımlılık mevcut kilitte bulunmuyor.

Ayrıca `stop()` sonrasında hemen `Ok(())` dönmek, kabul edilmiş bağlantıların boşalmasını beklemez.
`ListenerTasks::Drop`, boşaltmayı ayrı bir göreve bırakır.
Runtime kapandığında bu görev ve kalan bağlantılar sonlandırılabilir.
Doğru sinyal çözümü, açık bir bekleme noktası ve sınırlı kapanış süresi gerektirir.
Bu nedenle yüzeysel bir SIGTERM/SIGINT düzeltmesi eklenmedi.
Mevcut sinyal ile kapanış sınırı devam ediyor.

### Symlink izleme kapsamı

- Gerçek hedef, yapılandırılan yolun dizini dışında olsa da düzenleme algılanıyor.
- Geçersiz hedef içeriği mevcut nesli koruyor; içerik düzeltildiğinde reload çalışıyor.
- Symlink atomik biçimde başka bir mevcut hedefe yöneltildiğinde yeni hedef izleniyor.
- Yeni hedefteki sonraki düzenleme de algılanıyor.
- İzleyici yenilenirken kanala ulaşmış bir olay, sonraki reload turuna taşınıyor.
- Canonicalize başarısızsa yapılandırılan yol izlenmeye devam ediyor.
- Eksik veya erişilemeyen symlink hedefindeki her gelecekteki değişikliğin algılanacağı garanti edilmiyor.
- İzleyici yenilenemezse yeni nesil reddediliyor; mevcut nesil ve izleyici korunuyor.

### CI kapsamı

Kaynak kontrolü, `HEAD == GITHUB_SHA` yanında staged ve unstaged takip edilen dosya değişikliklerini reddeder.
Ignore edilen ve takip edilmeyen dosyaları bu kontrol kapsamaz.
Temiz, ayrılmış çalışma alanı ve güvenilen önbellek gereksinimi belgede açık tutuldu.

Runner kontrol trafiği, Docker daemon çıkışı, IPv4/IPv6, proxy ve DNS yolları için kontrol listesi eklendi.
Loopback testleri korunmalı; izolasyon denemelerinde VPS kullanılmamalıdır.
Runner kurulumu veya gerçek izolasyon denemesi bu görevin kapsamında yapılmadı.

### Bu turda değişen dosyalar

- `src/main.rs`
- `src/server_runtime.rs`
- `src/tcp/tcp_server.rs`
- `src/config/production.rs` — yalnız kapsam yorumu.
- `tests/cases/integration/server_lifecycle.rs`
- `CONFIG.md`
- `.github/workflows/test.yml`
- `.github/workflows/lint.yml`
- `.github/workflows/build.yml`
- `.github/workflows/mobile.yml`
- `.github/OFFLINE-CI.md`
- `report.md` — yalnız bu sonuç bölümü eklendi.

Önceki düzeltmeler ve ilgisiz kullanıcı değişiklikleri korundu.
`Cargo.toml`, `Cargo.lock`, shell profilleri ve istemci yönlendirmesi değiştirilmedi.
Staging, commit, push, canlı bakım veya dağıtım yapılmadı.

### Son doğrulama

İzole toolchain ve `--locked --offline` kullanıldı.
Derleme hedefi `/home/demir/.cache/shoes-review-target`, geçici test dizini `/tmp/opencode` oldu.

| Kontrol | Sonuç |
|---|---|
| Workspace kütüphane birim testleri | 925 geçti; 0 başarısız; 35 atlandı. |
| Uygulama birim testleri | 926 geçti; 0 başarısız; 35 atlandı. |
| Test destek paketi | 36 geçti; 0 başarısız. |
| Yerel entegrasyon paketi | 97 geçti; 0 başarısız; 0 atlandı. |
| Host FFI kütüphane testleri | 930 geçti; 0 başarısız; 35 atlandı. |
| Tüm hedef ve özelliklerle Clippy, `-D warnings` | Geçti. |
| `cargo fmt --all -- --check` | Geçti. |
| `git diff --check` | Geçti. |
| Workflow YAML ve offline komut politikası | Dört workflow için geçti. |
| Kirli takip edilen kaynakla CI guard kontrolü | Altı iş kontrolü kaynak ağacını doğru biçimde reddetti. |
| Manifest ve kilit değişmezliği | `Cargo.toml` ve `Cargo.lock` için diff yok. |

Yeni regresyonlar, symlink hedefi düzenleme/retargeting ve görev sonlanma nedenlerinin ayrımını kapsıyor.
Ortak kodu sınayan test hedeflerinin sayıları benzersiz test toplamı olarak toplanmamalıdır.

### Kalan sınırlar

Sinyal yönetimi, otomatik artık soket temizliği ve içerik değişmediğinde reload atlama eklenmedi.
Canlı sunucu, gerçek offline runner ve diğer platformlar üzerinde doğrulama yapılmadı.
Hiddify platform matrisi, tünel arızası sonrası sızıntı ve no-bypass uyumluluğu yeniden sınanmadı.
Karşılaştırmalı hız, CPU ve tahsis ölçümü yapılmadı.
Bu sonuçlar hız kazancı veya tam güvenlik kanıtı değildir.

---

# Uzun Ufuklu İnceleme (2026-10-13)

Yöntem: iki bağımsız model taraması (av) + çapraz doğrulama (her adayı diğer model doğruladı) +
iki bağımsız düzeltme + kapılar + yerel A/B. Kısıtlar: wire/format/fingerprint değişikliği yok;
stok Xray/Hiddify uyumu ve full-TUN TCP `whatsapp.net` SNI yolu korunur. Bu bölüm dağıtım,
bakım veya ayar değişikliği izni vermez. Değişiklikler commit edilmedi.

## Uygulanan düzeltmeler

| No | Düzeltme | Dosya | Test |
|---|---|---|---|
| LH-1 | `PacketAddrStream` teslim yolunda `put_slice` kapasite koruması (gizli panik; paket tüketilmeden `InvalidData`) | `src/uot/uot_v1_server_stream.rs` | `test_small_read_buffer_rejects_without_consuming_the_packet` |
| LH-2 | UdpRouter ping timer `MissedTickBehavior::Skip` (stall sonrası tick birikimi) | `src/routing/udp_router.rs` | `ping_timer_skips_missed_ticks_after_a_stall` |
| LH-3 | TCP/Unix accept red yoluna 100 ms throttle (bütçe doluyken accept+close busy-loop) | `src/tcp/tcp_server.rs` | mevcut limit testleri; zamanlayıcı testi eklenmedi |
| LH-4 | XUDP `wire_sessions` tavanı 16 384 + nesil-damgalı O(1) amortized tahliye | `src/xudp/message_stream.rs` | 4 test: tavan, tahliye rotaları, yenileme, kuyruk sınırı |
| LH-5 | `SpliceState` yanıltıcı yorumun düzeltilmesi (davranış değişikliği yok) | `src/linux_splice.rs` | — |
| LH-6 | h2mux per-connection stream tavanı: config yoksa **4096** (açık config kazanır) | `src/h2mux/h2mux_server_session.rs` | `omitted_stream_limit_falls_back_to_the_safety_ceiling` |
| LH-7 | UDP hedef tavanı: config yoksa **2048** hedef/akış (açık config kazanır) | `src/routing/udp_router.rs` | `omitted_udp_destination_limit_falls_back_to_the_safety_ceiling` |

Kapılar: `fmt` geçti; `clippy -D warnings` geçti; birim 970/0 (35 ignored); FFI 974/0.

Yerel A/B (release + `lto="fat"`, `black_box`, sıra değişimli):
- LH-1 guard: 0,5 → 0,7 ns/paket (+0,2 ns; gürültü seviyesi).
- LH-4 oturum başlatma: 32–34 → 56–58 ns/yeni oturum (+24 ns; paket başına değil, oturum kurulumu başına).

## Belgelenen politikalar (uygulanmadı — sahip kararı)

| No | Konu | Durum / öneri |
|---|---|---|
| LH-8 | DNS negatif sonuç cache'lenmiyor | Kısmen doğrulandı; kısa TTL (5–30 sn) negatif cache güvenli. XUDP'de çözümleme hatasının oturumu sonlandırması ayrı davranış kararı. Sahip kararı: uygulanmadı. |
| LH-9 | Splice pipe'ı idle'da bağlantı başına 4 FD tutuyor | Repro'lu; ölçüm + yön `Complete` olduğunda bırakma önerisi. Tam idle-release perf ölçümü ister. |
| LH-10 | REALITY post-handshake Handshake (KeyUpdate) kaydında bağlantıyı kapatıyor | Stok Go/utls istemcileri KeyUpdate göndermiyor; gerçek fix (trafik sırrı güncelleme) minimal değil → belge. |
| LH-11 | h2mux `poll_write` Ok(0) desenkronu, h2mux idle drain abort, reload grace drenaj birikimi | Düşük/erişilemez (`--no-reload`); belge. |

## Refute edilenler

- Vision unpadder kısmi UUID düşürmesi (ödünç semantiği; üretimde çağrı yolu yok).
- Sıfır kapasiteli `ReadBuf` → EOF karışıklığı (çağıranlar boş tampon üretmiyor).
- REALITY kapak 5 sn deadline (fail-closed tasarım; sahip kısıtı).
- Reload drenaj birikimi (`--no-reload` ile erişilemez).

## Bellek: periyodik heap trim (2026-10-13)

- Bulgu: 615 eşzamanlı bağlantılık stres sonrası RSS ~272 MB'ta kalıyor (glibc arena rezervi;
  ikinci aynı stres RSS'i artırmıyor → sızıntı yok). VPS 967 MB RAM.
- Ölçüm: `malloc_trim(0)` 256-300 MB heap'te 0,02–15 ms; belleği 261 MB → 5 MB iade ediyor;
  trim sonrası yeniden dokunma cezası ölçülemedi (0 fark).
- Çözüm: `src/memory.rs` — 5 dk aralıkla, yalnızca RSS > 64 MB **ve** aktif bağlantı < 128 iken,
  `spawn_blocking` içinde `libc::malloc_trim(0)`. Sıcak yola etkisi yok; wire/stok istemci etkisi yok.
  (Eşik 16→128: normal kullanım 20-40 bağlantı açtığı için 16 kapısı pratikte hiç açılmıyordu.)
- Testler: karar mantığı + VmRSS okuma + FFI smoke + (ignored) trim maliyet bench'i.

## Kararlar (2026-10-13)

- LH-1–LH-5 uygulandı ve kaldı; LH-6/LH-7 kod tavanı olarak uygulandı (4096 stream / 2048 hedef,
  açık config her zaman kazanır); LH-8–LH-11 belgelendi (kod değişikliği yok).

## Kod sadeleştirme paketi 1 (2026-10-13)

Üç bağımsız model incelemesi (GPT-6.1 Sol, Muse Spark, DS V4.1) + GPT değerlendirmesi
sonucu seçilen ilk üç madde; her biri ayrı onay + iki bağımsız testten geçti:

| No | Düzeltme | Dosya | Doğrulama |
|---|---|---|---|
| E01 | Etkisiz `bind_ip` parametre zinciri kaldırıldı | `src/tcp/tcp_server_handler_factory.rs`, `src/tcp/tcp_server.rs` | GPT onay + Muse/DS test |
| E06 | Çağrısız FFI log yardımcıları (`write_to_log_file`, `flush_log_file`) kaldırıldı | `src/ffi/common.rs` | GPT onay + Muse/DS test |
| E05 | Boş startup cleanup döngüsü `?` ile sadeleştirildi | `src/tcp/tcp_server.rs` | GPT şartlı onay + Muse/DS test |

Sıcak yol dokunulmadı; performans regresyonu riski düşük (A/B gerekli görülmedi).
Son aday kapıları: fmt, clippy `--all-features`, birim 1981/0, FFI 977/0, entegrasyon 97/0.
Kanıtlar: `~/.local/state/shoes-live/code-review-2026-10-13/`.

### TF01 — Destek testlerinde efemer-port yarışı

Tarih: 2026-10-13. Durum: **kapandı (2026-10-15, W2)**. Öncelik: orta (test güvenilirliği borcu; üretim regresyonu değil).
Düzeltme: port, bağlı ama listen etmeyen TCP soketiyle tutuluyor (gelen SYN → RST; port dış süreçlere kapalı).
Kanıt: 18 hücreli matris (boşta+yüklü, threads=1/4/8/16) **2.840 koşu, 0 başarısız, 0 TF01**; bağımsız Muse doğrulaması
(kernel deneyi 7/7 EADDRINUSE/ECONNREFUSED + 702 koşu 0 hata).

`port_helper::tests::readiness_uses_the_shared_deadline`, `tests/support/src/port_helper.rs:329`
üzerinde `unwrap_err()` çağrısına `Ok(())` gelince düşüyor. Test, geçici listener'ı kapattıktan
sonra bağlantı deniyor; paralel yerel testlerin aynı portu alması beklenen başarısız bağlantıyı
başarıya çevirebiliyor. Gözlenen hata süre assertion'ı değil, beklenmedik bağlantı başarısıdır.

Muse ve DS bağımsız yeniden üretti; DS aynı hata modunu `shoes` kodunu çalıştırmadan destek
paketinde üretti (E05 ile ilgisiz). Ölçümler: izole 0/100 ve 0/200; paket threads=8 1/100;
threads=16 1/20 (+2 ayrı `AddrInUse`). Test/helper dosyası değişmemiştir.

Ayrı görev: başarısız bağlantı/deadline senaryosunu paralel port yeniden kullanımından bağımsız
tasarlamak; testi atlamak, `Ok` sonucunu başarı saymak veya yeşile kadar tekrar etmek çözüm değildir.

## Kod sadeleştirme paketi 2 (2026-10-13)

Aynı incelemenin 4–7. adayları; her biri GPT onayı + Muse/DS bağımsız testlerinden geçti:

| No | Düzeltme | Dosya | Doğrulama |
|---|---|---|---|
| E36 | `validate_and_expand_selection` → `validate_selection` (yanıltıcı ad) | `src/config/validate.rs` | GPT onay + Muse/DS test |
| E03 | Kopya TLS deserializer birleştirildi (`Deserialize` + `deny_unknown_fields`) | `src/config/types/client.rs` | Şartlı onay → 3 koşul karşılandı (baseline provenance dahil) |
| E04 | TLS/REALITY target kurulumu iki helper'a çıkarıldı | `src/tcp/tcp_server_handler_factory.rs` | Şartlı onay → koşul 3 izleme görevine |
| E02 | Etkisiz constructor resolver taşıması kaldırıldı | `src/tcp/`, `src/dns/`, `src/tun/`, `src/client_proxy_selector.rs`, `src/routing/udp_router.rs` (11 dosya) | Şartlı onay → koşul 3 izleme görevine |

Sıcak yol dokunulmadı; runtime resolver sahipliği ve connect yolları korundu (15 kritik dosya byte-identical).
Son aday kapıları: workspace 1993, FFI 983, parse-only entegrasyon 4, fmt/clippy/check yeşil.

### E02/E04 test kapsamı takibi — açık, orta öncelik

E02 kaynak/sahiplik eşdeğerliği ve bağımsız çevrimdışı kapılarla kabul edildi; tam dinamik matris kapanmadı.
Ayrı runtime görevinde client-chain direct TCP/UDP ve proxy hostname yolları, DNS bootstrap-proxy tüketimi,
IPv4/IPv6 sayaçları, hata/literal ve TUN/router resolver kimliği sınansın. E04'ün taşıma/cover görevine
factory 0/1/N zincir içeriği, hop sayısı/sırası, UDP yeteneği ve override bağımsızlığı eklensin.
Aynı eski/yeni fixture'lar kullanılsın; kurulum DNS/connect yapmasın. Runtime testleri injected resolver ve
loopback ile sınırlı olsun; ağ/VPS erişimi yok. Bu kabul hız kazancı veya Hiddify platform uyumu kanıtı değildir.

## Kod sadeleştirme paketi 3 (2026-10-14)

| No | Düzeltme | Kapsam | Doğrulama |
|---|---|---|---|
| E11 | Kaldırılmış protokol, modül ve implementasyonlara ilişkin yanıltıcı yorumlar düzeltildi; Android `mixed` örneği VLESS oldu | 9 Rust dosyası; yalnız yorum | Şart karşılandı; nihai kabul; Muse/DS geçti |
| E20 | DoT/DoH/HTTP-3 ve hickory bağımlılık feature açıklaması düzeltildi | `src/dns/mod.rs`; yalnız yorum | Muse/DS geçti; feature/kurucu yolları doğrulandı |

Toplam 10 dosya, +20/−17; yorum dışı içerik byte-identical. fmt/clippy/birim/FFI kapıları geçti.
Workspace: lib 978, bin 979, support 36 başarılı; FFI 983 başarılı; başarısız test yok.
Ana lib/bin ve FFI özetlerindeki 36 ignored test çalıştırılmış sayılmadı.
Entegrasyon/canlı ağ/perf testi yapılmadı; hız veya tam güvenlik kazanımı iddiası yok. Bu paket için deploy gerekmez.

İsteğe bağlı takip: `Cargo.toml` keywords ve `src/client_proxy_chain.rs` mock etiketlerini güncel kapsamla hizalamak.
Bunlar runtime destek kanıtı değildir; mevcut kabulün koşulu değildir. Ret literal'leri ve tarihsel atıflar korunur.

## Ölçüm ve garanti kampanyası (2026-10-14)

Kod incelemesinin "Ölçüm sonrası" 6 adayı (E10, E14, E19, E26, E34, E35) ölçüldü; her ölçüm
bağımsız bir doğrulama ajanıyla yeniden üretildi. Sahip kuralı: test + **kesin performans
garantisi** olmadan hiçbir değişiklik tutulmaz.

| Madde | Ölçüm/garanti | Karar |
|---|---|---|
| E19 | Üretim `bin` ikilisi **bit-aynı** (bayt düzeyinde; 11 999 sembol stream'i eşit) | **Tutuldu** |
| E10 | Çekirdek gövde komut-eşdeğer; UDP çağıranı ve h2 flush monomorfizasyonları farklı | Düşürüldü (garanti yok) |
| E35 | TLS kurulumları + yaygın global kodgen kayması (XUDP/REALITY/h2 gövdeleri farklı) | Düşürüldü (garanti yok) |
| E14 | +%0,34 aktarım / +%0,79 kısa oturum; kontrol bandında (gürültü) | Uygulanmadı |
| E26 | Sıcak-poll kazancı yok; soyutlama büyüyor (+33 satır) | Uygulanmadı |
| E34 | Yazma −%6,4; parse +%1,4; C deneyi parse artışının dispatch'ten gelmediğini gösterdi (alan/yerleşim şüphelisi) | Ek ölçüm gerekli |

- E19'un üretimde bayt-nötrlüğü ayrıca doğrulandı: commit'li durum ile çalışma ağacı (E19 + testler +
  doküman) aynı deploy-profil ikilisini üretir (`5f1557e7…`).
- E02/E04 test kapsamı borcu 12 dinamik testle kapatıldı (client-chain resolver sayaçları, DNS
  bootstrap tüketimi, TUN/UDP-router kimliği, factory 0/1/N matrisi; determinizm 10/10).
- E09/E15/E23/E28 "belgele" kararları uygulandı: `CONFIG.md` (h2mux no-op havuz limitleri),
  `docs/konfigurasyon.md` (etkisiz QUIC alanları), `docs/sorun-giderme.md` (log marker önceliği).
- Ölçümler: tek makine (i5-1135G7), fat LTO, sıra değişimli turlar + A/A-B/B kontrolleri; canlı A/B yok.
  Toplam hızlanma iddiası yok; E19 yalnız kodgen-nötr bir bakım refaktörüdür.
- Düşürülen adayların patch'leri ve tüm ölçüm raporları yerel arşivde:
  `~/.local/state/shoes-live/measurements-2026-10-14/`.
- **Takip (2026-10-15):** E34 düşürüldü (C deneyi: parse artışı dispatch'ten değil; alan/layout + B'ye özgü
  kodgen; batch32 parse sinyali ve garanti kuralı altında kabul edilemez). İkinci temizlik paketi
  (E07/E08/E13/E30/E31) kodgen kapısında takıldı: deploy ikilisinde erişilebilir H2Mux/Vision/UDP/TUN
  fonksiyonlarında gerçek komut farkları → alınmadı (patch ve kanıt arşivde).

## Stable release 2026-10-14

**Tag:** `stable-2026-10-14` (bu bölümü içeren commit) · **Canlı binary:** `5f1557e7…`

### Kapsam
- İnceleme paketi 1: E01 (`bind_ip`), E06 (FFI log kalıntıları), E05 (startup cleanup) + TF01 kaydı.
- İnceleme paketi 2: E36 (ad), E03 (serde), E04 (target helper'ları), E02 (resolver taşıması) + E02/E04 test kapsamı takibi.
- İnceleme paketi 3: E11/E20 (yanıltıcı yorumlar + DNS feature belgesi).
- Ölçüm kampanyası: E19 (rustls makro; üretim bin bit-aynı kanıtı), E02/E04 için 12 dinamik test,
  E09/E15/E23/E28 belgele notları.

### Doğrulama (release anı)
- Kapılar: fmt, clippy `--all-targets --all-features -D warnings`, workspace 2023, FFI 998, entegrasyon 97/0.
- Canlı: `5f1557e7…`, active, NRestarts=0, açık genel IPv4 TCP 443 dinliyor;
  unit `8240b7ce…` + config `3b22cce9…` değişmedi.
- Rollback: `b853b7cd…` → `release-5f1557e7-1791999343/rollback.bin` (+receipt).

### Bu release'e alınmayanlar (bilinçli)
- E10/E35: kesin performans garantisi yok (üretim kodgen farkı) — patch'ler arşivde.
- E14/E26: ölçüm/analiz sonucu kazanç yok.
- E34: ek ölçüm gerekli (C deneyi parse artışının dispatch'ten gelmediğini gösterdi).
- İkinci temizlik paketi (E07/E08/E13/E30/E31/E33/E15): API/platform kapsam teyidi bekliyor.
- TF01 port flake'i, `examples/` temizliği, sinyal yönetimi ve LH-8…LH-11: kayıtlı borçlar.

## Belgele notları (2026-10-14)

- **E16:** `examples/README.md` legacy/upstream kapsamını açıklar; kaldırılmış protokol örnekleri destek kanıtı değildir. Silme/arşivleme yapılmadı.
- **E17:** `src/lib.rs` crate-geneli `allow(dead_code)` gerçek çağrısız kodu da maskeler. `src/ffi/mod.rs` desktop `ffi` derlemesindeki mobil ortak yardımcıları, `src/option_util.rs` ise yalnız bazı platformlarda kullanılan `is_one` kontrolünü kapsar. İzinler korundu; toplu kaldırma çok-platformlu doğrulama kapsamını büyütür.
- **E22:** `src/ffi/stub.rs::stub_warning` depo içinde çağrısızdır; desktop `ffi` feature'ıyla dışa açılan public API olduğu için korunur. Harici kullanım bilinmiyor.
- **E24:** Doğrulama/dönüşümden sonra kurulan zincir, sunucu ve soket yollarındaki invariant yorumları netleştirildi. Public yardımcıların doğrulamasız çağrılması için güvenlik garantisi verilmez; panic davranışı değişmedi.
- **E27:** `StreamReader` async I/O okuyucusu, `SlideBuffer` saf tampondur. Rolleri farklı olduğundan birleştirme reddedildi; ayrı kalırlar.
- **E32:** `TimeoutResolver::new` ve `DEFAULT_TIMEOUT` public API olarak korunur; harici tüketici olabilir. Depo içi kurulum `with_timeout` kullanır; dış kullanım doğrulanmış değildir.
