#!/usr/bin/env bash
# Установка restream (Rust) + MediaMTX на Debian/Ubuntu. Одинаково для VDS и для виртуалки.
# Запуск:  sudo bash install.sh
# Можно запускать повторно: ключи и твой config.ini не перезаписываются.
set -euo pipefail

[ "$(id -u)" -eq 0 ] || { echo "Запусти от root: sudo bash install.sh"; exit 1; }
command -v apt-get >/dev/null || { echo "Нужен Debian/Ubuntu (apt-get)."; exit 1; }

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONF_DIR=/etc/restream
CONF="$CONF_DIR/config.ini"
MTX_DIR=/etc/mediamtx
MTX_PORT=11935              # RTMP MediaMTX: слушает локально, наружу смотрит restream (:1935)
SRT_PORT=18890              # SRT MediaMTX: слушает локально, наружу смотрит restream (:8890)
FALLBACK_VERSION="v1.21.3"  # используется, только если не удалось узнать последнюю версию

echo "==> Пакеты (ffmpeg, curl, шрифт для заглушки)"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq ffmpeg curl ca-certificates openssl fonts-dejavu-core

echo "==> Пользователь restream"
id restream >/dev/null 2>&1 || useradd --system --create-home \
    --home-dir /var/lib/restream --shell /usr/sbin/nologin restream

echo "==> MediaMTX"
if [ ! -x /usr/local/bin/mediamtx ] || [ "${UPDATE_MEDIAMTX:-0}" = 1 ]; then
    case "$(uname -m)" in
        x86_64)        ARCH=amd64 ;;
        aarch64|arm64) ARCH=arm64 ;;
        armv7l)        ARCH=armv7 ;;
        *) echo "Неподдерживаемая архитектура: $(uname -m)"; exit 1 ;;
    esac
    VER="${MEDIAMTX_VERSION:-}"
    if [ -z "$VER" ]; then
        VER="$(curl -fsSL https://api.github.com/repos/bluenviron/mediamtx/releases/latest \
              | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' || true)"
        VER="${VER:-$FALLBACK_VERSION}"
    fi
    echo "    версия $VER, архитектура $ARCH"
    URL="https://github.com/bluenviron/mediamtx/releases/download/${VER}/mediamtx_${VER}_linux_${ARCH}.tar.gz"
    TMP="$(mktemp -d)"
    curl -fsSL "$URL" -o "$TMP/mtx.tgz"
    tar -xzf "$TMP/mtx.tgz" -C "$TMP" mediamtx
    install -m 755 "$TMP/mediamtx" /usr/local/bin/mediamtx
    rm -rf "$TMP"
else
    echo "    уже установлен (обновить: UPDATE_MEDIAMTX=1 sudo bash install.sh)"
fi

echo "==> Программа restream"
BIN="$SRC/restream"
if [ ! -x "$BIN" ]; then
    # На случай, если рядом с install.sh лежит только исходник:
    # собираем из каталога с Cargo.toml, если он есть.
    for cand in "$SRC/../target/release/restream" "$SRC/target/release/restream"; do
        [ -x "$cand" ] && { BIN="$cand"; break; }
    done
fi
if [ ! -x "$BIN" ]; then
    echo "    нет готового бинарника restream рядом с install.sh." >&2
    echo "    Собери его: cargo build --release   (затем скопируй target/release/restream в server/)" >&2
    exit 1
fi
install -d /opt/restream
install -m 755 "$BIN" /opt/restream/restream
ln -sf /opt/restream/restream /usr/local/bin/restream

echo "==> Конфиг"
install -d -m 750 -o root -g restream "$CONF_DIR"
if [ ! -f "$CONF" ]; then
    LIVE_KEY="$(openssl rand -hex 8)"
    TEST_KEY="$(openssl rand -hex 8)"
    cat > "$CONF" <<EOF
[general]
# Путь, на который телефон шлёт RTMP. Это и есть "ключ" — никому не показывай.
live_path = live/${LIVE_KEY}
fallback_file = /var/lib/restream/fallback.mp4

# Сколько секунд телефон должен быть стабильно на связи, чтобы вернуться на него
live_confirm_seconds = 2
# Через сколько минут заглушки без возврата телефона эфир останавливается (0 = никогда)
fallback_max_minutes = 30
# Тишина в сокете дольше этого = обрыв связи (LTE пропал)
stall_timeout = 6.0
poll_interval = 0.5

# Приём: restream слушает этот порт, MediaMTX работает за ним
listen_host = 0.0.0.0
listen_port = 1935
mediamtx_host = 127.0.0.1
mediamtx_port = ${MTX_PORT}
mediamtx_rtmp = rtmp://127.0.0.1:${MTX_PORT}
mediamtx_api = http://127.0.0.1:9997
control_host = 127.0.0.1
control_port = 8088

# --- Приём SRT (тот же эфир, но по SRT). Выключается: srt_enabled = no ---
srt_enabled = yes
srt_listen_host = 0.0.0.0
srt_listen_port = 8890
srt_upstream_host = 127.0.0.1
srt_upstream_port = ${SRT_PORT}
# Вывод ffmpeg в логи (no — только предупреждения и ошибки, yes — всё подряд)
quiet_ffmpeg = no

# --- Выходы. Каждый включённый [output.*] = отдельный ffmpeg. ---

# Локальная проверка без Twitch/Kick (смотреть: mpv rtmp://IP:1935/test/${TEST_KEY}).
# На боевом сервере можно выключить: enabled = no
[output.test]
enabled = yes
url = rtmp://127.0.0.1:${MTX_PORT}/test/${TEST_KEY}

[output.twitch]
enabled = no
url = rtmp://live.twitch.tv/app/ВСТАВЬ_КЛЮЧ_TWITCH

[output.kick]
enabled = no
url = rtmps://ВСТАВЬ_ИНГЕСТ_KICK.global-contribute.live-video.net:443/app/ВСТАВЬ_КЛЮЧ_KICK
EOF
    echo "    создан $CONF"
else
    echo "    $CONF уже есть — не трогаю"
fi
chown root:restream "$CONF"
chmod 640 "$CONF"

# Конфиг прежней версии знал stop_path и не знал параметров прокси.
if ! grep -q '^listen_port' "$CONF"; then
    echo "    конфиг от прежней версии — дописываю параметры прокси"
    sed -i '/^stop_path/d' "$CONF"
    sed -i 's#/test/#/test/#' "$CONF"
    python3 - "$CONF" "$MTX_PORT" "$SRT_PORT" <<'PY'
import sys
conf, port, srt_port = sys.argv[1], sys.argv[2], sys.argv[3]
text = open(conf).read()
add = f"""listen_host = 0.0.0.0
listen_port = 1935
mediamtx_host = 127.0.0.1
mediamtx_port = {port}
mediamtx_rtmp = rtmp://127.0.0.1:{port}
mediamtx_api = http://127.0.0.1:9997
stall_timeout = 6.0
srt_enabled = yes
srt_listen_host = 0.0.0.0
srt_listen_port = 8890
srt_upstream_host = 127.0.0.1
srt_upstream_port = {srt_port}
"""
head, sep, rest = text.partition("\n[")
open(conf, "w").write(head.rstrip("\n") + "\n" + add + sep + rest)
text = open(conf).read().replace("127.0.0.1:1935/test/", f"127.0.0.1:{port}/test/")
open(conf, "w").write(text)
PY
    chown root:restream "$CONF"; chmod 640 "$CONF"
fi

echo "==> Конфиг MediaMTX (генерируется из $CONF)"
install -d "$MTX_DIR"
python3 - "$MTX_PORT" "$SRT_PORT" <<'PY'
import configparser, re, sys
port, srt_port = sys.argv[1], sys.argv[2]
cp = configparser.ConfigParser(interpolation=None)
cp.read("/etc/restream/config.ini")
g = cp["general"]
paths = [g["live_path"].strip("/")]
# Тестовый выход публикуется в MediaMTX, значит путь должен быть разрешён.
m = re.search(r"rtmp://127\.0\.0\.1:%s/(.+)$" % re.escape(port),
              cp.get("output.test", "url", fallback=""))
if m:
    paths.append(m.group(1).strip())
rt = g.get("mediamtx_read_timeout", "6s").strip()
srt_on = "yes" if g.get("srt_enabled", "yes").strip().lower() in ("yes", "true", "1", "on") else "no"
yml = f"""# Сгенерировано install.sh из /etc/restream/config.ini — руками не правь.
logLevel: info
# Если телефон молчит дольше этого — MediaMTX считает его отвалившимся.
readTimeout: {rt}
writeTimeout: 10s

# Управляющий API — только localhost (restream проверяет готовность потока)
api: yes
apiAddress: 127.0.0.1:9997

# Наружу ничего не торчит: RTMP слушает только localhost, перед ним стоит restream.
rtsp: no
hls: no
webrtc: no
moq: no
rtmp: yes
rtmpAddress: 127.0.0.1:{port}

# SRT тоже только на localhost: снаружи его принимает restream (:8890)
srt: {srt_on}
srtAddress: 127.0.0.1:{srt_port}

# Принимаются только эти пути; любые другие — отказ.
paths:
""" + "".join("  %s: {}\n" % p for p in paths)
open("/etc/mediamtx/mediamtx.yml", "w").write(yml)
PY

echo "==> Заглушка"
if [ ! -f /var/lib/restream/fallback.mp4 ]; then
    runuser -u restream -- /usr/local/bin/restream -c "$CONF" make-fallback
fi

echo "==> systemd"
cat > /etc/systemd/system/mediamtx.service <<'EOF'
[Unit]
Description=MediaMTX (RTMP ingest for restream)
After=network-online.target
Wants=network-online.target

[Service]
User=restream
ExecStart=/usr/local/bin/mediamtx /etc/mediamtx/mediamtx.yml
Restart=always
RestartSec=2
LimitNOFILE=65536
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF
cat > /etc/systemd/system/restream.service <<'EOF'
[Unit]
Description=restream (RTMP relay with fallback to Twitch/Kick)
After=network-online.target mediamtx.service
Wants=network-online.target mediamtx.service

[Service]
User=restream
ExecStart=/opt/restream/restream -c /etc/restream/config.ini run
Restart=always
RestartSec=2
TimeoutStopSec=10
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable mediamtx.service restream.service >/dev/null
systemctl restart mediamtx.service
sleep 1
systemctl restart restream.service
sleep 2

LIVE="$(sed -n 's/^live_path *= *//p' "$CONF")"
TEST="$(sed -n 's#^url *= *rtmp://127.0.0.1:[0-9]*/test/##p' "$CONF" | head -1)"

echo
systemctl --no-pager --lines=0 status mediamtx restream | grep -E 'Active|●' || true
cat <<EOF

Готово. Путь для Larix (хост и порт — публичный адрес этой машины):
  ТРАНСЛЯЦИЯ : rtmp://ХОСТ:1935/${LIVE}
  ТО ЖЕ ПО SRT: srt://ХОСТ:8890?streamid=publish:${LIVE}
  ПРОСМОТР ТЕСТОВОГО ВЫХОДА (проверка): rtmp://ХОСТ:1935/test/${TEST}

Трансляцию останавливает кнопка Stop в Larix — отдельный адрес для этого не нужен.
Если телефон пропал без нажатия Stop, включается заглушка и эфир продолжается.

Дальше: sudo nano ${CONF}  (вставь ключи Twitch/Kick, enabled = yes)
        sudo systemctl restart restream
Логи:   journalctl -u restream -f
EOF
