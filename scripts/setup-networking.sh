#sudo ip tuntap add veth0 mode tap
curl -X PUT -i \
  --unix-socket /tmp/firecracker.socket \
  http://localhost/network-interfaces/eth0 \
  -H accept:application/json \
  -H content-type:application/json \
  -d '{
    "iface_id": "eth0",
    "host_dev_name": "fc-41-tap0",
    "guest_mac" : "52:54:00:12:34:56"
}'
#    "guest_mac" : "52:54:00:12:34:56"
#    "host_dev_name": "veth0",
