# Operasyon Runbook'u

Bu belge mevcut sunucu için binary dağıtımı ve geri dönüş prosedürünün esas kaynağıdır.
**Kural:** Dağıtım, canlı bakım, yeniden başlatma ve ayar değişikliği için önce
sahip onayı alınır. APT işlemleri elle yapılır; otomatik yeniden başlatma kapalıdır.
Hazırlık veya derleme, canlı dağıtım izni değildir.
Otomatik dağıtım aracı yoktur (2026-10-16 sahip kararı); dağıtım, sınırlı sağlık gözlemi
ve geri dönüş elle yapılır. Bu adımlar bu sunucunun mevcut systemd düzeni içindir;
genel bir dağıtım şablonu değildir.
Bu rehber yeni VPS kurulumu, unit/config değişikliği veya istemci yönlendirme değişikliği yapmaz.
Bu rehber yalnız kaynak/doküman tutarlılığı ve shell sözdizimi açısından kontrol edildi;
canlı VPS'te çalıştırılmadı ve canlıda doğrulanmış sayılmaz.

## Sunucu düzeni (VPS)

| Öğe | Değer |
|---|---|
| Servis | `shoes.service` (kullanıcı: `xray`) |
| Çalıştırma | `/usr/local/bin/shoes --no-reload --production-profile /usr/local/etc/shoes/config.yaml` |
| Yapılandırma | `root:xray`, mod **640** — okunmaz, kopyalanmaz, raporlanmaz; yalnız SHA-256 tutulur |
| Bakım dizini | `/var/lib/shoes-maintenance/<id>/` (rollback + receipt) |
| Birim yedeği | `unit-profile-*` dizinindeki `shoes.service.before` |
| Dinleyici | Yalnız açık genel IPv4 TCP 443 |
| Kaynaklar | Önceki kayıt: ~1 GB RAM; `LimitNOFILE=65536`; dağıtım öncesi ayrıca kontrol edilir |

`<VPS_IP>` yer tutucudur; gerçek adres ve SSH ayrıntıları yalnız yerel kayıtta
(`~/.pi/agent/server-ssh.json`).

## 1. Ön koşullar ve yerel hazırlık

- [CONTRIBUTING.md](../CONTRIBUTING.md) kabul kontrolleri tamamlanmış olmalı.
- Önceden hazırlanmış Linux hedefi için release derlemesi yapılmalı; upstream binary kullanılmaz.
- Kaynak commit'i, varsa çalışma ağacı farkının kimliği, toolchain/hedef ve binary SHA-256 özeti yerel teslim kaydına yazılır.
- SSH host anahtarı önceden doğrulanmış olmalı. Alias, kullanıcı, port ve anahtar yerel SSH yapılandırmasında tutulur.
- Yönetim arayüzü bu laptop'ta `wlp0s20f3` olmalı; başka cihazda gerçek arayüz açıkça seçilir.
- VPS'te mevcut servis/dizin düzeni ve `sudo` erişimi gerekir. Bu rehber sudoers kuralı oluşturmaz.
- VPS araçları: GNU coreutils (`sha256sum`, `timeout` dahil), `awk`, `ss`, `install`, `mv`, systemd.
  Yerel makinede OpenSSH ve GNU coreutils gerekir. Araç kurulmaz, indirme yapılmaz.
- Sahneleme ve rollback için yeterli disk alanı, ayrıca bakım penceresi operatör tarafından doğrulanır.
- Günlükler tanılamadır ancak gizli üst veri içerebilir: genel günlük temizliği otomatik
  parola/anahtar redaksiyonu değildir ve hedefli gizlilik düzeltmeleri tüm günlüklerin
  gizlisiz olduğunu garanti etmez. Ham günlük ve config içeriği raporlara eklenmez;
  config yalnız hash için okunur, yazdırılmaz, kopyalanmaz, `--dry-run`'a verilmez.

Derleme ortamı için [geliştirme rehberini](../CONTRIBUTING.md#toolchain) kullanın.
Varsayılan belgelenen hedef diziniyle binary: `$HOME/.cache/shoes-target/release/shoes`.

## 2. Yerel derleme ve SHA-256

Release derlemesi ağsız, komut-yerel ortamla yapılır; binary özeti onay kaydına yazılır:

```bash
CARGO_HOME="$HOME/.local/share/shoes-toolchain/cargo" \
RUSTUP_HOME="$HOME/.local/share/shoes-toolchain/rustup" \
PATH="$HOME/.local/share/shoes-toolchain/cargo/bin:$PATH" \
CARGO_TARGET_DIR="$HOME/.cache/shoes-target" \
cargo build -p shoes --bin shoes --release --locked --offline

sha256sum -- "$HOME/.cache/shoes-target/release/shoes"
```

Aday eser bu özetle sabitlenir; yerel hash ile VPS'te ölçülen hash eşleşmezse dağıtım durur.

## 3. Elle dağıtım (yalnız ayrıca onaylı)

Adımlar ayrı ayrı çalıştırılır; her doğrulama geçmeden sonraki adıma geçilmez.
Yer tutucular: `<ID>` = bu dağıtıma özel kimlik (`release-YYYYMMDDTHHMMSSZ-<12 hex>`),
`<APPROVED_SHA256>` = onaylı binary özeti, `<SSH_ALIAS>` = yerel SSH alias'ı,
`<PUBLIC_IPV4>` = açık dinleme adresi. Kimlik bilgileri komuta, kayda veya rapora yazılmaz.

1. Onaylı yönetim SSH bağlantısıyla benzersiz ve özel yükleme dizinini oluşturup adayı aktarın.
   Aynı bağlı seçenekler kullanılır; `-p` yoktur, dizin zaten varsa zincir durur ve aktarım yapılmaz:

```bash
ssh -o BindInterface=wlp0s20f3 -o BatchMode=yes -o StrictHostKeyChecking=yes \
  -- '<SSH_ALIAS>' 'umask 077; mkdir -m 700 -- /var/tmp/shoes-upload-<ID>' \
  && scp -o BindInterface=wlp0s20f3 -o BatchMode=yes -o StrictHostKeyChecking=yes \
    -- "$HOME/.cache/shoes-target/release/shoes" '<SSH_ALIAS>:/var/tmp/shoes-upload-<ID>/shoes.new'
```

2. Bağlı yönetim oturumunu açın; aşağıdaki VPS bloklarını bu oturumda çalıştırın:

```bash
ssh -o BindInterface=wlp0s20f3 -o BatchMode=yes -o StrictHostKeyChecking=yes -- '<SSH_ALIAS>'
```

Oturumda ilk olarak `pipefail` etkinleştirin; bu ayar yalnız oturuma uygulanır,
kabuk profilleri değiştirilmez. Böylece boru hattındaki bir üst komut hatası gizlenmez:

```bash
set -o pipefail
```

Sıfır dışı dönen her komut veya boru hattı **DUR** demektir; sonraki adıma geçilmez.

3. Bu oturumda temel durumu ölçün ve değişkenlerde tutun. Servis sağlıksızsa, çalışan binary
   hash'i canlı binary ile eşleşmiyorsa veya dinleyici beklenenden farklıysa **durup dağıtım yapmayın**:

```bash
sudo systemctl is-active --quiet shoes.service && echo active
systemctl show shoes.service -p NRestarts -p User --value
sudo sha256sum -- "/proc/$(systemctl show shoes.service -p MainPID --value)/exe"
sudo ss -H -ltnp4 'sport = :443'
sudo ss -H -ltn6 'sport = :443'
live_sha=$(sha256sum -- /usr/local/bin/shoes | awk '{print $1}')
config_sha=$(sudo sha256sum -- /usr/local/etc/shoes/config.yaml | awk '{print $1}')
unit_text_sha=$(systemctl cat shoes.service | sha256sum | awk '{print $1}')
unit_props_sha=$(systemctl show shoes.service --property=User --property=Group --property=Environment \
  --property=EnvironmentFiles --property=DropInPaths --property=FragmentPath \
  --property=TimeoutStopUSec --property=KillMode | sha256sum | awk '{print $1}')
printf 'live=%s\nconfig=%s\nunit_text=%s\nunit_props=%s\n' "$live_sha" "$config_sha" "$unit_text_sha" "$unit_props_sha"
```

Bu dört değer aynı SSH oturumunda tutulur ve adım 5 kaydında kullanılır. Kayıt için
değişiklikten sonra yeniden ölçülmez; yalnız karşılaştırmada yeniden hesaplanır.
Config yalnız hashlenir; içerik yazdırılmaz.

4. Bakım dizinini hazırlayıp adayı doğrulayın; sürüm dizini her dağıtımda yeni ve benzersizdir.
   `mkdir` var olan dizinde başarısız olur; zincir orada durur, mevcut sürüm üzerine yazılmaz:

```bash
sudo install -d -o root -g root -m 700 -- /var/lib/shoes-maintenance \
  && sudo mkdir -m 700 -- '/var/lib/shoes-maintenance/<ID>' \
  && sudo install -o root -g root -m 755 -- '/var/tmp/shoes-upload-<ID>/shoes.new' '/var/lib/shoes-maintenance/<ID>/shoes.new' \
  && sudo sha256sum -- '/var/lib/shoes-maintenance/<ID>/shoes.new'   # <APPROVED_SHA256> olmalı
```

`shoes.new` hash'ini onaylı yerel değerle elle karşılaştırın. Eşleşmiyorsa durun;
aşağıdaki `rollback.bin` kopyasına geçmeyin. Eşleşiyorsa mevcut binary'nin kopyasını alın:

```bash
sudo install -o root -g root -m 755 -- /usr/local/bin/shoes '/var/lib/shoes-maintenance/<ID>/rollback.bin' \
  && sudo sha256sum -- '/var/lib/shoes-maintenance/<ID>/rollback.bin'
```

5. Kayda yalnız hash ve durum alanlarını yazın; unit/config metni yazılmaz.
   `config_sha`, `unit_text_sha` ve `unit_props_sha` adım 3 temel ölçümünden aynı oturumda taşınır;
   değişiklikten sonra alınan yeni ölçümler kayda yazılmaz:

```bash
before=$(sudo sha256sum -- '/var/lib/shoes-maintenance/<ID>/rollback.bin' | awk '{print $1}')
printf 'timestamp_utc=%s\nstatus=prepared\nprevious_binary_sha256=%s\ncandidate_binary_sha256=%s\nconfig_sha256=%s\nunit_text_sha256=%s\nunit_properties_sha256=%s\n' \
  "$(date -u +%FT%TZ)" "$before" '<APPROVED_SHA256>' "$config_sha" "$unit_text_sha" "$unit_props_sha" | \
  sudo tee '/var/lib/shoes-maintenance/<ID>/receipt.txt' >/dev/null
```

6. Taşımadan önce config ve unit değişmediğini doğrulayın; kardeş `.new` yolu doluysa
   veya bir symlink ise zincir durur ve kurulum yapılmaz:

```bash
sudo sha256sum -- /usr/local/etc/shoes/config.yaml      # config_sha ile aynı olmalı
systemctl cat shoes.service | sha256sum                 # unit_text_sha ile aynı olmalı
systemctl show shoes.service --property=User --property=Group --property=Environment \
  --property=EnvironmentFiles --property=DropInPaths --property=FragmentPath \
  --property=TimeoutStopUSec --property=KillMode | sha256sum   # unit_props_sha ile aynı olmalı
sudo sha256sum -- /usr/local/bin/shoes                  # live_sha ile aynı olmalı
[[ ! -e '/usr/local/bin/shoes.<ID>.new' && ! -L '/usr/local/bin/shoes.<ID>.new' ]] \
  && sudo install -o root -g root -m 755 -- '/var/lib/shoes-maintenance/<ID>/shoes.new' '/usr/local/bin/shoes.<ID>.new' \
  && sudo sha256sum -- '/usr/local/bin/shoes.<ID>.new'  # <APPROVED_SHA256> olmalı
```

Aday hash'i onaylı yerel değerle elle karşılaştırılır. Eşleşmiyorsa `.new` dosyası taşınmaz;
durun ve inceleyin. Eşleşiyorsa atomik taşımayı ayrı çalıştırın:

```bash
sudo mv -T -- '/usr/local/bin/shoes.<ID>.new' /usr/local/bin/shoes
```

Unit/config değişmişse veya canlı binary üçüncü bir hash'e değişmişse taşıma yapılmaz; durup inceleyin.
Unit özeti, unit/drop-in metni ile seçili sabit yüklü özelliklerdir. `ExecStart`'ın PID ve zaman
damgası taşıyan çalışma gösterimi bilinçli olarak dışlanır: bu alanlar restart'ta değişir ve kalıcı
ayar değildir. `ExecStart`'ın bildirimi unit dosyası metniyle kapsanır; dış EnvironmentFile
içerikleri kapsam dışıdır. Ham unit metni çıktıya veya kayda yazılmaz. İki unit digest'i restart
sonrası yinelenip temel değerlerle karşılaştırılır.

7. Restart'ı sınırlı süreyle çalıştırın:

```bash
sudo timeout --kill-after=5 60 systemctl restart shoes.service
```

`timeout` 60 saniyede SIGTERM gönderir; yalnızca 5 saniye daha gerekirse `--kill-after` ile
SIGKILL uygular. systemd işinin iptal edildiği varsayılmaz; sonuç, sağlık gözlemiyle doğrulanır.

Unit, config, istemci ve sistem ayarları değiştirilmez; `daemon-reload`/`reset-failed` çalıştırılmaz.
Geçici yükleme dosyası başarılı dağıtımdan sonra ayrıca onaylı bakımda temizlenir; bakım yedekleri silinmez.

### Sağlık kabulü ve kapsamı

Sınırlı sağlık gözlemi: aşağıdaki kontroller 60 saniyelik gözlem bütçesi içinde en az 10 saniye
art arda başarılı olmalıdır. İlk başarısızlıkta durun ve elle geri dönün. Kontrolleri kısa
aralıklarla yineleyin:

```bash
systemctl is-active --quiet shoes.service && echo active
systemctl show shoes.service -p NRestarts -p MainPID --value
sudo sha256sum -- "/proc/$(systemctl show shoes.service -p MainPID --value)/exe"   # <APPROVED_SHA256> olmalı
sudo sha256sum -- /usr/local/etc/shoes/config.yaml   # config_sha ile aynı olmalı
systemctl cat shoes.service | sha256sum              # unit_text_sha ile aynı olmalı
systemctl show shoes.service --property=User --property=Group --property=Environment \
  --property=EnvironmentFiles --property=DropInPaths --property=FragmentPath \
  --property=TimeoutStopUSec --property=KillMode | sha256sum   # unit_props_sha ile aynı olmalı
sudo ss -H -ltnp4 'sport = :443'    # yalnız <PUBLIC_IPV4>:443; soket MainPID'e ait olmalı
sudo ss -H -ltn6 'sport = :443'     # çıktı boş olmalı
timeout --kill-after=2 2 bash -c 'exec 3<>"/dev/tcp/<PUBLIC_IPV4>/443"'
```

- Servis `active`; `NRestarts=0`; çalışan `MainPID`'nin yürütülebilir dosya hash'i aday binary ile eşleşir.
- IPv4 TCP 443 üzerinde yalnız beklenen açık genel adres dinler; soket servis `MainPID`'sine aittir.
- IPv6 TCP 443 dinleyicisi bulunmaz; beklenen IPv4:443 adresine TCP bağlantısı kurulabilir.
- Config hash'i ve iki unit digest'i değişmemiştir; restart sonrası yeniden hesaplanıp temel değerlerle karşılaştırılır.

`ss` çıktısı tek başına yeterli değildir. Bu, sunucu üzerindeki yerel TCP kabul kontrolüdür;
dış ağ erişimini, REALITY/Vision el sıkışmasını, UDP/XUDP aktarımını veya Hiddify platform
uyumunu kanıtlamaz. Tam tünel kontrolü ayrıca onaylı kapsam ister.

Gözlem geçtiyse kayıt durumunu güncelleyin; geçmediyse kaydı değiştirmeden elle geri dönün:

```bash
sudo sed -i 's/^status=prepared$/status=deployed/' '/var/lib/shoes-maintenance/<ID>/receipt.txt'
```

## 4. Hata ve elle geri dönüş

Otomatik rollback yoktur (2026-10-16 sahip kararı). Kurulum veya sağlık gözlemi başarısızsa
operatör aşağıdaki adımlarla eski binary'yi geri kurar ve sağlık kabulünü yineler.
Config/unit beklenmedik şekilde değişmişse eski binary geri kurulabilir; ancak bu ayarlarla
yeniden başlatma yapılmaz. Canlı binary üçüncü bir hash'e değişmişse üzerine yazılmaz.

| Kayıt durumu | Anlamı / işlem |
|---|---|
| `prepared` | Sahneleme tamamlandı; kurulum ve sağlık henüz doğrulanmadı. Sonuç belirsizse canlı binary hash'ine bak. |
| `deployed` | Aday kuruldu ve sağlık kabulünü geçti. |
| `rolled_back` | Operatör eski binary'yi geri kurdu ve sağlık kabulünü geçti. |
| `rollback_failed` | Elle kurtarma tamamlanamadı; döngüsel restart yapma, olayı kaydet. |

Disk hatasında kayıt yazılamayabilir. SSH kaybı, reboot veya güç kaybında durum belirsizdir;
kontrol edilmeden yeniden dağıtım denenmez.

### Manuel kurtarma

Bu adımlar yalnız onaylı bakımda, bağlı yönetim SSH oturumunda kullanılır.
`<DIR>`, hatalı dağıtımdan önceki binary'yi içeren doğrulanmış bakım dizinidir; son dizini rastgele seçmeyin.
Önce başka dağıtım olmadığını, `rollback.bin` hash'inin kayıt ve yerel onay ile eşleştiğini
ve unit/config'in değişmediğini doğrulayın. Ön koşul sağlanmıyorsa durun; yapılandırmayı tahmin ederek düzeltmeyin.

```bash
# VPS'te; yer tutucu yerine doğrulanmış bakım dizinini yazın.
sudo bash -s -- '<DIR>' <<'SH'
set -euo pipefail
dir=$1
expected=$(awk -F= '$1 == "previous_binary_sha256" { print $2 }' "$dir/receipt.txt")
[[ $expected =~ ^[a-f0-9]{64}$ ]]
[[ $(sha256sum -- "$dir/rollback.bin" | awk '{print $1}') == "$expected" ]]
temporary="/usr/local/bin/shoes.rollback-$$"
[[ ! -e $temporary && ! -L $temporary ]]
install -o root -g root -m 755 -- "$dir/rollback.bin" "$temporary"
[[ $(sha256sum -- "$temporary" | awk '{print $1}') == "$expected" ]]
mv -T -- "$temporary" /usr/local/bin/shoes
timeout --kill-after=5 60 systemctl restart shoes.service
SH
```

Ardından yukarıdaki sağlık kabulünü ve eski binary hash'ini doğrulayın; başarılıysa kaydı işaretleyin:

```bash
sudo sed -i 's/^status=.*$/status=rolled_back/' '<DIR>/receipt.txt'
```

Servis hâlâ sağlıksızsa döngüsel restart yapmayın; ham günlük ve config raporlara
eklenmeden yalnız yerel inceleyin, kaydı `rollback_failed` yapıp olayı bildirin.

## 5. Yedek saklama

Sahip kararı (2026-10-15; 2026-10-16'da yinelendi): dağıtım sırasında bakım kayıtları otomatik silinmez.
Temizlik ayrı onaylı işlemdir. Çalışan sürümün kaydı, doğrulanmış kullanılabilir `rollback.bin`
ve gerekli `shoes.service.before` yedeği korunur. Toplu silme komutu bu rehberin parçası değildir.

## Yönetim SSH kuralları

- TUN, VPS adresini kendi üzerinden yönlendirebilir; bu yüzden SSH **WiFi
  arayüzüne bağlanır** (`-o BindInterface=wlp0s20f3`). Bu, servis restart'ında
  yönetim yolunu tünelden ayırır; hotspot veya SSH kesintisine karşı garanti değildir.
- Uzun oturumlarda `ServerAliveInterval`/`ServerAliveCountMax` kullanılır
  (hotspot kopmalarında asılı kalmayı önler).
- Kimlik bilgileri komut satırına gömülmez; yerel anahtar dosyası kullanılır.

## İzleme

Mevcut operatöre özel script'ler (`~/.local/state/shoes-live/`; depoya dahil değildir).
Genel dağıtım akışı bunlara bağımlı değildir:

| Script | Nerede çalışır | Ölçtüğü |
|---|---|---|
| `server-monitor.sh` | VPS (salt okunur) | CPU %, RSS, thread, eşzamanlı 443 bağlantısı, backlog, retrans paket/bayt, ERROR sayısı |
| `client-probe.sh` | Laptop | Tünel RTT (ping) + gstatic TTFB (tünel üzerinden) |

Tarihsel ölçümler (2026-10-13): CPU %0,1–2,8; backlog sürekli 0; TTFB
~0,24–0,32 sn; ping ~20–50 ms (mobil yola göre). Bu değerler garanti değil,
gözlem kaydıdır.

## Bellek operasyonu

- Bağlantı patlaması sonrası allocator serbest sayfaları tutabilir; RSS yüksek kalabilir.
  Sabit RSS tek başına sızıntı olmadığını kanıtlamaz.
- `src/memory.rs`, glibc heap trim dener (5 dk, RSS > 64 MB, bağlantı < 128).
  Bu çağrı Linux CLI'ın jemalloc ile ayırdığı Rust belleğini temizlemez;
  native bağımlılıkların glibc tahsisleri etkilenebilir.
- Yerel kodun CLI politikası (`src/allocator.rs`, yalnız GNU/Linux CLI):
  `dirty_decay_ms=60000`, `muzzy_decay_ms=0`, arka plan temizliği açık.
  Dolu `_RJEM_MALLOC_CONF` varsa yerleşik varsayılanlar atlanır.
  Bu, kesin 60 sn iade süresi veya RSS üst sınırı değildir; eşikli açma/kapama yoktur.
  Yerleşik politikanın kurulum hatası, dinleyiciler açılmadan başlangıcı durdurur.
  Bu yerel değişiklik canlıya dağıtım değildir; devreye alma ayrıca onay gerektirir.
- Acil durumda RSS sıfırlamak için onaylı yeniden başlatma yapılabilir
  (servis restart'ı bağlantıları keser; kısa kesinti).

## Acil durum kontrol listesi

1. Servis durumu: `systemctl is-active shoes`, `NRestarts`.
2. Dinleyici: `ss -H -ltnp4 'sport = :443'`; adres/PID ve TCP kabulünü ayrıca doğrula.
3. Son hatalar: `journalctl -u shoes --since "-5 min" -o cat`.
4. Kaynak: CPU/RSS/thread, eşzamanlı bağlantı, backlog (server-monitor.sh).
5. Gerekirse rollback (yukarıdaki adım) — sonra sağlık kontrolü.
6. Yaşanan olayı yerel kayda işle (kimlik bilgisi ve ham config olmadan).

## CI ve ağ politikası

- CI **çevrimdışıdır**; tüm işler elle (`workflow_dispatch`), önceden hazırlanmış
  self-hosted runner'larda çalışır; kod indirmez, ağ erişimi yapmaz.
- **Hiçbir workflow VPS'e bağlanmaz.**
- Ayrıntı: [.github/OFFLINE-CI.md](../.github/OFFLINE-CI.md).

## İlgili belgeler

- Deploy sonrası ölçüm: [performans-bellek.md](performans-bellek.md)
- Sorun giderme: [sorun-giderme.md](sorun-giderme.md)
