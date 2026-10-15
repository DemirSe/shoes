# Yapılandırma

Tam YAML referansı: [CONFIG.md](../CONFIG.md). Bu belge yalnızca bu fork'un
üretim profilini ve kritik davranışları özetler.

`examples/` yalnız desteklenen profillerin örneklerini içerir (VLESS/TLS, REALITY, TUN, direct); kapsam için [örnek notuna](../examples/README.md) bakın.

## Yapı

- Üst düzey liste; her öğe bir **sunucu**, **TUN**, **istemci** veya **kural**
  grubudur.
- Sunucular: `address`/`path` + `protocol` + `transport` (varsayılan `tcp`) +
  `rules` (varsayılan: tümüne doğrudan izin).
- Gruplar: `client_group`, `rule_group`; adlandırılmış PEM'ler: `pem`.

## Üretim profili (`--production-profile`)

Canlı birim bu bayrakla çalışır. Doğrulayıcı (`src/config/production.rs`)
şunları zorunlu kılar:

| Kural | Ayrıntı |
|---|---|
| Dinleme adresi | Yalnız **açık genel IPv4 TCP 443**. |
| Taşıyıcı | `transport: udp` reddedilir; UDP/XUDP TCP tüneli içinde kalır. |
| REALITY hedefi/SNI | Tam olarak `whatsapp.net`; başka kapak veya alt alan adı yok. |
| VLESS fallback | Opsiyonel fallback yapılandırılmaz. |
| Kapak davranışı | Kapak başarısızsa bağlantı durur (fail-closed). |

Genel doğrulama (`src/config/validate.rs`) bunun dışında: geçersiz yapılandırma
reload'da mevcut nesli korur; `--dry-run` geçersiz yapılandırmada sıfır dışı kod
döner.

## Kimlik doğrulama

- Sunucu `user_id` (tek) veya `user_ids` (liste) kabul eder.
- Doğrulama **yalnız bağlantı kurulumunda** yapılır; veri yolunda maliyet yoktur.
- Vision yanıtı, doğrulanan UUID'yi kullanır.
- Boş, geçersiz veya yinelenen UUID listeleri reddedilir.
- Paylaşılan UUID'nin **on cihaz** sınırı yönetseldir; otomatik cihaz sayımı
  yoktur ve istenmez.

## Limitler ve tavanlar

- `global_limits` (bağlantı/akış/QUIC bayt bütçeleri) isteğe bağlıdır.
- Yapılandırma sınır vermezse güvenlik tavanları uygulanır:
  H2MUX 4096 akış/bağlantı, UDP 2048 hedef/akış, XUDP 16 384 tel oturumu.
- **Açık yapılandırma her zaman kazanır.**

### Etkisiz kabul edilen QUIC alanları

Şu `global_limits` alanları doğrulamadan geçer ve mevcut profilleri bozmamak
için korunur; bu fork'ta üretim davranışına bağlı değildir:

- `quic_socket_buffer`: hiçbir soket kurucusuna aktarılmaz; soket tamponları
  sistem varsayılanında kalır.
- `quic_receive_window`, `quic_send_window`, `quic_stream_window`: pencereleri
  uygulayan kod yalnız testlerde derlenip çağrılır; üretim QUIC taşıyıcısı bu
  alanları okumaz. Değerler doğrulanır, taşıyıcıya uygulanmaz.

Alanları silmek (`deny_unknown_fields`) mevcut profilleri reddettirebilir;
etkinleştirmek davranış değişikliğidir ve sahip kararı + ölçüm gerektirir.

## Örnek iskelet (yer tutucular)

Gerçek değerler (adres, UUID, anahtar, short_id) burada **yer tutucudur** ve
asla depoya yazılmaz:

```yaml
- address: "<PUBLIC_IPV4>:443"          # üretim profilinde: açık genel IPv4 TCP 443
  protocol:
    type: tls
    reality_targets:
      "whatsapp.net":                    # üretim profilinde zorunlu tek kapak
        private_key: "<REALITY_PRIVATE_KEY>"
        short_ids: ["<SHORT_ID>", ""]
        dest: "whatsapp.net:443"
        protocol:
          type: vless
          user_id: "<UUID>"              # veya user_ids: ["<UUID1>", "<UUID2>"]
          udp_enabled: true
```

## TUN (istemci tarafı)

- Linux: `device_name`; Android/iOS: `device_fd`.
- TUN değişiklikleri canlıda **uygulanamaz**; onaylı yeniden başlatma gerekir
  (`server_runtime` TUN canlı değişimini reddeder).
- Sahip kuralı: TUN açık, bypass kuralı yok.

## İlgili belgeler

- Doğrulama kodları: `src/config/validate.rs`, `src/config/production.rs`
- Kararlar: [kararlar.md](kararlar.md)
- Operasyon: [operasyon.md](operasyon.md)
