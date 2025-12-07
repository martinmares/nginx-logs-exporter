# TEST

```bash

mkdir -p .tmp/nginx-logs

cat >> .tmp/nginx-logs/access.log <<'EOF'
127.0.0.1 - - [05/Dec/2025:12:00:00 +0100] "GET /ok HTTP/1.1" 200 123 "-" "curl/8.0" "-" upstream_status=200 upstream_addr=10.0.0.10:443 request_time=0.015
127.0.0.1 - - [05/Dec/2025:12:00:01 +0100] "GET /bad HTTP/1.1" 502 321 "-" "curl/8.0" "-" upstream_status=502 upstream_addr=10.0.0.11:443 request_time=0.450
10.220.24.33 - tsm-change-management [07/Dec/2025:09:11:56 +0000] "GET /_cluster/health/ HTTP/1.1" 200 416 "-" "elasticsearch-java/8.15.5 (Java/21.0.9)" "-" upstream_status=200 upstream_addr=172.29.188.7:9200 request_time=0.001 upstream_label=es-backend
10.220.24.33 - tsm-change-management [07/Dec/2025:09:11:56 +0000] "GET /_cluster/health/ HTTP/1.1" 200 416 "-" "elasticsearch-java/8.15.5 (Java/21.0.9)" "-" upstream_status=200 upstream_addr=172.29.188.7:9200 request_time=0.001 upstream_label=es-backend
10.220.15.59 - - [07/Dec/2025:09:11:56 +0000] "GET /_cluster/health/ HTTP/1.1" 200 416 "-" "elasticsearch-java/8.15.5 (Java/21.0.9)" "-" upstream_status=200 upstream_addr=172.29.188.7:9200 request_time=0.001 upstream_label=es-backend
10.220.26.2 - - [07/Dec/2025:09:11:56 +0000] "GET /_cluster/health HTTP/1.1" 200 416 "-" "kube-probe/1.31" "-" upstream_status=200 upstream_addr=172.29.188.7:9200 request_time=0.001 upstream_label=es-backend
EOF

touch .tmp/nginx-logs/error.log

ACCESS_LOG_PATH=$(pwd)/.tmp/nginx-logs/access.log \
ERROR_LOG_PATH=$(pwd)/.tmp/nginx-logs/error.log \
METRICS_PREFIX="tsm_es_" \
LISTEN_ADDR=127.0.0.1:9100 \
NGINX_UI_PUBLIC_PREFIX="" \
NGINX_UI_TITLE="es-proxy-logs" \
cargo run


```
