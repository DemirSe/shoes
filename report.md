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

## Kararlar (2026-10-13)

- LH-1–LH-5 uygulandı ve kaldı; LH-6/LH-7 kod tavanı olarak uygulandı (4096 stream / 2048 hedef,
  açık config her zaman kazanır); LH-8–LH-11 belgelendi (kod değişikliği yok).
