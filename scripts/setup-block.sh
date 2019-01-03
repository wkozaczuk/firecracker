curl --unix-socket /tmp/firecracker.socket -i \
    -X PUT 'http://localhost/drives/rootfs' \
    -H 'Accept: application/json'           \
    -H 'Content-Type: application/json'     \
    -d '{
        "drive_id": "rootfs",
        "path_on_host": "/home/wkozaczuk/projects/osv/build/release/usr.zfs",
        "is_root_device": false,
        "is_read_only": false
    }'
