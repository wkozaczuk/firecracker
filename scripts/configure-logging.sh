curl --unix-socket /tmp/firecracker.socket -i \
 -X PUT "http://localhost/logger" \
 -H "accept: application/json" \
 -H "Content-Type: application/json" \
 -d '{ "log_fifo": "log.fifo", "metrics_fifo": "metrics.fifo", "level": "Info", "show_level": true, "show_log_origin": true }'
