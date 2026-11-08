#!/usr/bin/env bash
# Публичный TCP-туннель к порту 1935 через openport — ТОЛЬКО для тестов на домашней ВМ.
# На VDS с белым IP не нужен.  Запуск (внутри ВМ):  sudo bash tunnel-openport.sh
set -euo pipefail
[ "$(id -u)" -eq 0 ] || { echo "Запусти от root: sudo bash tunnel-openport.sh"; exit 1; }

PORT="${TUNNEL_PORT:-1935}"

if ! command -v openport >/dev/null; then
    echo "==> Устанавливаю openport"
    curl -fsSL https://openport.io/download/debian64/latest.deb -o /tmp/openport.deb
    dpkg -i /tmp/openport.deb || apt-get -f install -y
    rm -f /tmp/openport.deb
fi
OP="$(command -v openport)"

# --ip-link-protection=False ОБЯЗАТЕЛЬНО: по умолчанию openport пускает на порт только тех,
# кто предварительно открыл секретную ссылку в браузере — Larix так не умеет.
cat > /etc/systemd/system/restream-tunnel.service <<EOF
[Unit]
Description=Public TCP tunnel to RTMP :${PORT} (openport)
After=network-online.target
Wants=network-online.target

[Service]
User=restream
Environment=HOME=/var/lib/restream
ExecStart=${OP} ${PORT} --ip-link-protection=False
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable restream-tunnel.service >/dev/null
systemctl restart restream-tunnel.service

echo "==> Жду адрес туннеля (до 40 с)"
LINE=""
for _ in $(seq 1 20); do
    sleep 2
    LINE="$(journalctl -u restream-tunnel -n 80 --no-pager 2>/dev/null | grep -i 'forwarding remote port' | tail -1 || true)"
    [ -n "$LINE" ] && break
done
if [ -n "$LINE" ]; then
    echo "$LINE"
    echo
    echo "Публичный адрес для Larix: rtmp://<host:port из строки выше>/<live_path из config.ini>"
    echo "Узнать путь:  sudo grep '^live_path' /etc/restream/config.ini"
else
    echo "Адрес пока не появился. Смотри: journalctl -u restream-tunnel -n 50"
fi
echo "Порт может смениться после перезапуска туннеля — адрес всегда смотри в: journalctl -u restream-tunnel | grep forwarding"
