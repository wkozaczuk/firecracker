curl --unix-socket /tmp/firecracker.socket -i \
    -X PUT 'http://localhost/boot-source'   \
    -H 'Accept: application/json'           \
    -H 'Content-Type: application/json'     \
    -d '{
        "kernel_image_path": "./loader-stripped-64.elf",
        "boot_args": "--ip=eth0,169.254.0.165,255.255.255.252 --defaultgw=169.254.0.166 /hello"
    }'
