# Örneklerin kapsamı

Bu dizinde yalnız güncel fork'un desteklediği profillerin örnekleri tutulur.
Kaldırılmış protokolleri (VMess, Trojan, Shadowsocks/ShadowTLS, HTTP/SOCKS,
Hysteria2, TUIC, AnyTLS, NaiveProxy, Snell vb.) veya kaldırılmış aktarımları
(WebSocket, QUIC) gösteren legacy/upstream örnekleri silindi; bu protokoller
yeniden etkinleştirilmez.

Kalan örnekler:

- `reality_basic.yaml`, `reality_vision.yaml` — REALITY + VLESS sunucu
- `vless_over_tls.yaml` — TLS + VLESS sunucu
- `tun_vpn.yaml` — TUN + direct istemci

Örneklerdeki sertifika/anahtar yolları ile REALITY `private_key` değerleri
tutucudur; gerçek değerlerle değiştirilmelidir. REALITY örnekleri genel kaps
alan adları kullanır; üretim profilinde kapak/SNI tam olarak `whatsapp.net`
olmalıdır. Üretim profili için
[CONFIG.md](../CONFIG.md) ve [üretim profili özeti](../docs/konfigurasyon.md)
esas alınmalıdır.
