#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# Bind only to localhost and use ephemeral ports. Credentials are test-only.
containers=()
cleanup() {
    for container in "${containers[@]}"; do
        docker rm --force "$container" >/dev/null 2>&1 || true
    done
}
trap cleanup EXIT

start_broker() {
    local container ready=false
    container=$(docker run --detach --rm -p 127.0.0.1::1883 eclipse-mosquitto:2 sh -c '
    mosquitto_passwd -b -c /tmp/passwd han-test test-password
    mosquitto_passwd -b /tmp/passwd han-updated new-password
    chmod 644 /tmp/passwd
    printf "listener 1883\nallow_anonymous false\npassword_file /tmp/passwd\npersistence false\n" > /tmp/test.conf
    exec mosquitto -c /tmp/test.conf
')
    containers+=("$container")

    for attempt in {1..50}; do
        if docker exec "$container" mosquitto_pub -h localhost -u han-test -P test-password -t readiness -m ready 2>/dev/null; then
            ready=true
            break
        fi
        sleep 0.2
    done
    if [[ "$ready" != true ]]; then
        docker logs "$container"
        return 1
    fi
}

start_broker
start_broker
HAN_TEST_BROKER=$(docker port "${containers[0]}" 1883/tcp) \
HAN_TEST_BROKER_SECOND=$(docker port "${containers[1]}" 1883/tcp) \
    cargo test --locked -p han-mqtt --test broker -- --ignored --nocapture
