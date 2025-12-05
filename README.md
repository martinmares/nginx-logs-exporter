# TEST

```bash

mkdir -p .tmp/nginx-logs

cat >> .tmp/nginx-logs/access.log <<'EOF'
127.0.0.1 - - [05/Dec/2025:12:00:00 +0100] "GET /ok HTTP/1.1" 200 123 "-" "curl/8.0" "-" upstream_status=200 upstream_addr=10.0.0.10:443 request_time=0.015
127.0.0.1 - - [05/Dec/2025:12:00:01 +0100] "GET /bad HTTP/1.1" 502 321 "-" "curl/8.0" "-" upstream_status=502 upstream_addr=10.0.0.11:443 request_time=0.450
EOF

touch .tmp/nginx-logs/error.log

ACCESS_LOG_PATH=$(pwd)/.tmp/nginx-logs/access.log \
ERROR_LOG_PATH=$(pwd)/.tmp/nginx-logs/error.log \
LISTEN_ADDR=127.0.0.1:9100 \
NGINX_UI_PUBLIC_PREFIX="" \
NGINX_UI_TITLE="es-proxy-logs" \
cargo run


```
