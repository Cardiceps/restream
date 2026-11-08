#!/usr/bin/env bash
# Тестовая виртуалка для restream на Arch Linux: чистый QEMU/KVM, без libvirt и Docker.
# Debian 13 (cloud-образ) + cloud-init. Сеть user-mode (NAT): ВМ ходит в интернет сама,
# с хоста доступна по SSH на 127.0.0.1:2222, а порт 1935 хоста проброшен в ВМ.
#
#   ./vm.sh create     скачать образ и создать диск
#   ./vm.sh start      запустить ВМ (в фоне)
#   ./vm.sh deploy     поставить в ВМ restream + MediaMTX (server/install.sh)
#   ./vm.sh tunnel     поставить в ВМ публичный TCP-туннель (openport)
#   ./vm.sh ssh [cmd]  зайти в ВМ / выполнить команду
#   ./vm.sh status | stop | destroy
set -euo pipefail

DIR="${VM_DIR:-$HOME/vm/restream}"
IMG_URL="${IMG_URL:-https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2}"
BASE="$DIR/$(basename "$IMG_URL")"
DISK="$DIR/restream.qcow2"
SEED="$DIR/seed.iso"
PIDFILE="$DIR/qemu.pid"
KNOWN="$DIR/known_hosts"
SSH_PORT="${SSH_PORT:-2222}"
RTMP_PORT="${RTMP_PORT:-1935}"
RTMP_BIND="${RTMP_BIND:-}"          # пусто = слушать на всех интерфейсах хоста (нужно для теста с телефона по Wi-Fi)
MEM="${MEM:-2048}"
CPUS="${CPUS:-2}"
DISK_SIZE="${DISK_SIZE:-20G}"
VM_USER=stream
SERVER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../server" && pwd)"

die()  { echo "Ошибка: $*" >&2; exit 1; }
need() { for c in "$@"; do command -v "$c" >/dev/null || die "не найдена команда '$c' (см. README: установка пакетов)"; done; }
is_running() { [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; }

vm_ssh() {
    # shellcheck disable=SC2086
    ssh ${SSH_EXTRA:-} -p "$SSH_PORT" -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$KNOWN" \
        -o LogLevel=ERROR -o ConnectTimeout=3 "$VM_USER@127.0.0.1" "$@"
}

wait_ssh() {
    echo -n "Жду загрузку ВМ (первый раз 1–2 минуты)"
    for _ in $(seq 1 60); do
        if vm_ssh true 2>/dev/null; then echo " — готово."; return 0; fi
        echo -n "."; sleep 3
    done
    echo
    die "ВМ не ответила по SSH. Лог консоли: $DIR/console.log"
}

cmd_create() {
    need qemu-img qemu-system-x86_64 xorriso curl ssh
    mkdir -p "$DIR"
    [ ! -f "$DISK" ] || die "ВМ уже создана ($DISK). Пересоздать: ./vm.sh destroy"

    PUBKEY=""
    for k in "$HOME/.ssh/id_ed25519.pub" "$HOME/.ssh/id_rsa.pub" "$HOME/.ssh/id_ecdsa.pub"; do
        [ -f "$k" ] && { PUBKEY="$(cat "$k")"; break; }
    done
    [ -n "$PUBKEY" ] || die "нет SSH-ключа. Создай: ssh-keygen -t ed25519"

    if [ ! -f "$BASE" ]; then
        echo "Скачиваю образ Debian: $IMG_URL"
        curl -fL --progress-bar -o "$BASE.part" "$IMG_URL"
        mv "$BASE.part" "$BASE"
    fi

    qemu-img create -q -f qcow2 -F qcow2 -b "$BASE" "$DISK" "$DISK_SIZE"

    mkdir -p "$DIR/seed"
    cat > "$DIR/seed/meta-data" <<EOF
instance-id: restream-$(date +%s)
local-hostname: restream
EOF
    cat > "$DIR/seed/user-data" <<EOF
#cloud-config
users:
  - name: $VM_USER
    shell: /bin/bash
    groups: [adm, systemd-journal]
    sudo: ALL=(ALL) NOPASSWD:ALL
    lock_passwd: true
    ssh_authorized_keys:
      - $PUBKEY
ssh_pwauth: false
EOF
    xorriso -as mkisofs -quiet -V cidata -J -r -o "$SEED" "$DIR/seed" 2>/dev/null
    echo "ВМ создана в $DIR (диск $DISK_SIZE, ${MEM} МБ ОЗУ, $CPUS vCPU). Дальше: ./vm.sh start"
}

cmd_start() {
    need qemu-system-x86_64 ssh
    [ -f "$DISK" ] || die "сначала ./vm.sh create"
    if is_running; then echo "ВМ уже запущена (pid $(cat "$PIDFILE"))."; return 0; fi
    [ -w /dev/kvm ] || die "/dev/kvm недоступен: включи виртуализацию (VT-x) в BIOS и добавь себя в группу kvm: sudo usermod -aG kvm \$USER (потом перелогинься)"
    qemu-system-x86_64 \
        -name restream -machine q35,accel=kvm -cpu host -smp "$CPUS" -m "$MEM" \
        -drive if=virtio,file="$DISK",format=qcow2 \
        -drive if=virtio,file="$SEED",format=raw,readonly=on \
        -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:${SSH_PORT}-:22,hostfwd=tcp:${RTMP_BIND}:${RTMP_PORT}-:1935" \
        -device virtio-net-pci,netdev=n0 \
        -device virtio-rng-pci \
        -display none -serial "file:$DIR/console.log" \
        -daemonize -pidfile "$PIDFILE"
    wait_ssh
}

cmd_deploy() {
    is_running || die "ВМ не запущена: ./vm.sh start"
    echo "Копирую server/ в ВМ и запускаю install.sh ..."
    tar -C "$SERVER_DIR" -cz --exclude=__pycache__ . \
        | vm_ssh 'rm -rf ~/restream-src && mkdir ~/restream-src && tar -xz -C ~/restream-src && sudo bash ~/restream-src/install.sh'
}

cmd_tunnel() {
    is_running || die "ВМ не запущена: ./vm.sh start"
    [ -f "$SERVER_DIR/tunnel-openport.sh" ] || die "нет $SERVER_DIR/tunnel-openport.sh"
    vm_ssh 'sudo bash -s' < "$SERVER_DIR/tunnel-openport.sh"
}

cmd_stop() {
    is_running || { echo "ВМ не запущена."; return 0; }
    vm_ssh 'sudo poweroff' 2>/dev/null || true
    for _ in $(seq 1 20); do is_running || { echo "ВМ выключена."; rm -f "$PIDFILE"; return 0; }; sleep 1; done
    echo "Не выключилась сама — завершаю принудительно."
    kill "$(cat "$PIDFILE")" || true
    rm -f "$PIDFILE"
}

cmd_destroy() {
    read -r -p "Удалить диск ВМ и всё, что в ней (restream, конфиги, ключи)? [y/N] " a
    [ "$a" = y ] || [ "$a" = Y ] || { echo "Отмена."; return 0; }
    cmd_stop
    rm -rf "$DISK" "$SEED" "$DIR/seed" "$KNOWN" "$DIR/console.log"
    echo "Удалено (скачанный образ остался: $BASE)."
}

case "${1:-}" in
    create)  cmd_create ;;
    start)   cmd_start ;;
    deploy)  cmd_deploy ;;
    tunnel)  cmd_tunnel ;;
    ssh)     shift; is_running || die "ВМ не запущена"; SSH_EXTRA=-t vm_ssh "$@" ;;
    status)  if is_running; then echo "запущена (pid $(cat "$PIDFILE"))"; else echo "остановлена"; fi ;;
    stop)    cmd_stop ;;
    destroy) cmd_destroy ;;
    *) sed -n '2,12p' "$0"; exit 1 ;;
esac
